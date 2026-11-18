use {
    std::{
        ffi::OsStr,
        io,
        iter,
        path::Path,
        process::{ExitStatus, Stdio},
        time::{Duration, Instant},
    },
    log::warn,
    tokio::{
        io::{AsyncRead, AsyncReadExt},
        process::{Child, Command},
        runtime::Handle,
        time::{timeout, timeout_at},
    },
    crate::error::{Error, Result},
};

/// Longest wait for a killed child to exit
pub const KILL_GRACE: Duration = Duration::from_millis(500);

/// The bounds on one run of a command by [`CommandRunner::run`]
#[derive(Clone, Copy)]
pub struct RunLimits {
    /// Longest wait for the command to exit and close its output
    pub time: Duration,
    /// Most bytes of output read; longer output is an error
    pub output: u64,
}

/// Runs the program of one source, one run at a time. A killed process can
/// stay in uninterruptible sleep, as nvidia-smi does in a hung driver, so a
/// killed child that has not exited by the end of [`KILL_GRACE`] is kept here
/// and the program is not started again until it exits. A program that hangs
/// thus leaves at most one process per runner.
#[derive(Default)]
pub struct CommandRunner {
    /// The last child, killed and not yet exited
    retained: Option<Child>,
}

impl CommandRunner {
    /// Run `program` with `args`, its stdin closed and its stdout piped, and
    /// return its exit status and its output, which must be at most
    /// `limits.output` bytes. If the program has not exited and closed its
    /// output within `limits.time`, [`Error::CommandTimeout`] is returned and
    /// the child is killed. The call returns within about `limits.time` plus
    /// [`KILL_GRACE`]. While the child of an earlier run outlives its kill,
    /// the program is not started and [`Error::CommandStillRunning`] is
    /// returned.
    ///
    /// The call blocks the thread on the current tokio runtime, so on a
    /// runtime thread it must run inside `tokio::task::block_in_place`.
    pub fn run(&mut self, program: &Path, args: &[&OsStr], limits: RunLimits) -> Result<(ExitStatus, Vec<u8>)> {
        let runtime = Handle::try_current()
            .map_err(|e| Error::Io { path: program.to_owned(), source: io::Error::other(e) })?;

        runtime.block_on(self.run_async(program, args, limits))
    }

    async fn run_async(&mut self, program: &Path, args: &[&OsStr], limits: RunLimits)
        -> Result<(ExitStatus, Vec<u8>)>
    {
        if !self.release_exited() {
            return Err(Error::CommandStillRunning { command: command_text(program, args) });
        }

        let io_error = |e| Error::Io { path: program.to_owned(), source: e };
        let deadline = Instant::now()
            .checked_add(limits.time)
            .ok_or_else(|| io_error(io::Error::new(io::ErrorKind::InvalidInput,
                format!("time limit {:?} is too long", limits.time))))?;

        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(io_error)?;
        // Returning drops the child, which kills it
        let stdout = child.stdout.take()
            .ok_or_else(|| io_error(io::Error::other("stdout is not piped")))?;

        // When the deadline passes, the read is dropped with its end of the
        // pipe, also when a descendant of the child still holds the other end
        let output = read_capped(stdout, limits.output);
        let status = child.wait();
        let finished = timeout_at(deadline.into(), async move { tokio::join!(output, status) }).await;

        match finished {
            Ok((output, Ok(status))) => Ok((status, output.map_err(io_error)?)),
            Ok((_, Err(e))) => {
                self.stop(child, program).await;
                Err(io_error(e))
            }
            Err(_) => {
                self.stop(child, program).await;
                Err(Error::CommandTimeout {
                    command: program.display().to_string(),
                    seconds: limits.time.as_secs(),
                })
            }
        }
    }

    /// Kill `child`, a run of `program`, and wait at most [`KILL_GRACE`] for it
    /// to exit
    async fn stop(&mut self, mut child: Child, program: &Path) {
        // Kill fails if the child has already exited, which is harmless
        let _ = child.start_kill();
        let exited = timeout(KILL_GRACE, child.wait()).await.ok();

        self.after_kill(child, exited, program);
    }

    /// Keep `child`, a killed run of `program`, if `exited`, the outcome of the
    /// bounded wait for it, is `None` because the wait ran out
    fn after_kill(&mut self, child: Child, exited: Option<io::Result<ExitStatus>>, program: &Path) {
        if exited.is_some() {
            return;
        }

        warn!("{} did not exit within {:?} of being killed and may be stuck; it is not started again \
            for this source until it exits", program.display(), KILL_GRACE);

        self.retained = Some(child);
    }

    /// Drop the kept child if it has exited. Returns whether no child is kept.
    pub fn release_exited(&mut self) -> bool {
        let Some(child) = &mut self.retained else {
            return true;
        };

        // An error means there is no child left to wait for
        if let Ok(None) = child.try_wait() {
            return false;
        }

        self.retained = None;
        true
    }
}

#[cfg(test)]
impl CommandRunner {
    /// Keep `child` as if it were a run of `program` that outlived its kill,
    /// which no test can make, since SIGKILL cannot be ignored
    pub fn keep(&mut self, child: Child, program: &Path) {
        self.after_kill(child, None, program);
    }
}

/// Name a run by its program and arguments, each quoted, so that the log
/// says which disk or GPU it was for
fn command_text(program: &Path, args: &[&OsStr]) -> String {
    iter::once(program.as_os_str())
        .chain(args.iter().copied())
        .map(|part| format!("{:?}", part))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Read all of `pipe`, failing once it yields more than `cap` bytes. One byte
/// past the cap is read so that a cut output, whose prefix may still parse,
/// is never returned.
async fn read_capped(pipe: impl AsyncRead + Unpin, cap: u64) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    pipe.take(cap.saturating_add(1)).read_to_end(&mut output).await?;

    if output.len() as u64 > cap {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            format!("output is longer than {} bytes", cap)));
    }

    Ok(output)
}

/// Helpers for tests that run processes
#[cfg(test)]
pub mod testing {
    use {
        std::{
            env,
            fs,
            os::unix::fs::PermissionsExt,
            path::{Path, PathBuf},
            process::{self, Command, Stdio},
            sync::{
                atomic::{AtomicUsize, Ordering},
                Mutex,
                MutexGuard,
            },
            thread,
            time::{Duration, Instant},
        },
        tokio::{runtime::Builder, task},
    };

    /// Longest a test waits for a script's file or a child's state to appear
    pub const APPEAR_WAIT: Duration = Duration::from_secs(10);

    /// Limit for a run that must time out after its script has recorded a
    /// pid. It is far above any scheduling delay, so the script records the
    /// pid before the deadline even on a loaded host.
    pub const READY_LIMIT: Duration = Duration::from_secs(10);

    /// Directory under the system temp dir that is removed when dropped
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new() -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);

            let name = format!("ipmi-fan-control-test-{}-{}",
                process::id(), COUNTER.fetch_add(1, Ordering::SeqCst));
            let path = env::temp_dir().join(name);
            fs::create_dir(&path).unwrap();

            Self(path)
        }

        pub fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // Serialises the tests that spawn a process. A fork in another test
    // thread can inherit a script's write descriptor for a moment, and exec
    // of the script then fails with ETXTBSY.
    static SCRIPT_LOCK: Mutex<()> = Mutex::new(());

    pub fn script_lock() -> MutexGuard<'static, ()> {
        SCRIPT_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write `body` as an executable script `name` in `dir`
    pub fn write_script(dir: &TempDir, name: &str, body: &str) -> PathBuf {
        let script = dir.write(name, &format!("#!/bin/sh\n{}\n", body));
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// Run `f` as the zone loop runs an update: inside `block_in_place` on a
    /// multi-thread runtime
    pub fn blocking<T>(f: impl FnOnce() -> T) -> T {
        let runtime = Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();

        runtime.block_on(async { task::block_in_place(f) })
    }

    /// Poll `ready` until it gives a value or [`APPEAR_WAIT`] passes
    pub fn poll_until<T>(mut ready: impl FnMut() -> Option<T>) -> Option<T> {
        let deadline = Instant::now() + APPEAR_WAIT;

        loop {
            if let Some(value) = ready() {
                return Some(value);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// The pid a script wrote to `path`, if it wrote one
    pub fn written_pid(path: &Path) -> Option<String> {
        let contents = fs::read_to_string(path).ok()?;
        let pid = contents.trim();
        (!pid.is_empty()).then(|| pid.to_owned())
    }

    /// Wait until the process `pid` has no /proc entry. A zombie still has
    /// one, so this also waits for the process to be reaped.
    pub fn wait_until_gone(pid: &str) -> bool {
        let entry = Path::new("/proc").join(pid);
        poll_until(|| (!entry.exists()).then_some(())).is_some()
    }

    /// Kills the process whose pid a script wrote to a file, when dropped. A
    /// script that never wrote the file leaves nothing to kill.
    pub struct KillOnDrop(pub PathBuf);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let Some(pid) = written_pid(&self.0) else {
                return;
            };
            let _guard = script_lock();
            let _ = Command::new("sh")
                .args(["-c", "kill -9 \"$1\"", "sh", &pid])
                .stderr(Stdio::null())
                .status();
            wait_until_gone(&pid);
        }
    }

    /// Spawn a child that runs until killed, as a stand-in for one that
    /// outlived its kill, which a test cannot make
    pub fn spawn_sleeper() -> tokio::process::Child {
        tokio::process::Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    /// Body of a script that records its pid in `pid_file` and sleeps until
    /// killed. exec keeps the shell's pid, so the pid is the sleeping process.
    pub fn sleeping_body(pid_file: &Path) -> String {
        format!("echo $$ > '{}'\nexec sleep 30", pid_file.display())
    }

    /// Body of a script that starts a descendant, records the descendant's
    /// pid in `pid_file`, prints a reading and exits. The descendant sleeps
    /// until killed and keeps the script's stdout open.
    pub fn descendant_body(pid_file: &Path) -> String {
        format!("sleep 30 &\necho $! > '{}'\necho 71", pid_file.display())
    }
}

#[cfg(test)]
mod tests {
    use {
        std::{
            ffi::OsStr,
            fs,
            io,
            path::{Path, PathBuf},
            time::{Duration, Instant},
        },
        tokio::{
            process::Command,
            runtime::{Builder, Handle},
            task,
        },
        super::{
            CommandRunner,
            KILL_GRACE,
            RunLimits,
            testing::{
                blocking,
                descendant_body,
                poll_until,
                script_lock,
                sleeping_body,
                spawn_sleeper,
                wait_until_gone,
                write_script,
                written_pid,
                KillOnDrop,
                TempDir,
                READY_LIMIT,
            },
        },
        crate::error::Error,
    };

    /// Limit for a run that must finish, far above any scheduling delay
    const FINISH_LIMIT: Duration = Duration::from_secs(10);

    /// Limit for a run that must time out
    const TIMEOUT_LIMIT: Duration = Duration::from_secs(1);

    /// Time a bounded call may take beyond its bound on a loaded host
    const RETURN_SLACK: Duration = Duration::from_secs(5);

    const CAP: u64 = 4096;

    fn limits(time: Duration) -> RunLimits {
        RunLimits { time, output: CAP }
    }

    fn sh(script: &str) -> [&OsStr; 2] {
        [OsStr::new("-c"), OsStr::new(script)]
    }

    #[test]
    fn run_inside_block_in_place_on_a_worker_gives_output_and_times_out() {
        let _guard = script_lock();
        let runtime = Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();

        let (output, timed_out) = runtime.block_on(async {
            tokio::spawn(async {
                task::block_in_place(|| {
                    let mut runner = CommandRunner::default();
                    let output = runner.run(Path::new("sh"), &sh("echo hi"), limits(FINISH_LIMIT));
                    let timed_out = runner.run(Path::new("sleep"), &[OsStr::new("30")],
                        limits(Duration::from_millis(300)));

                    (output, timed_out)
                })
            }).await.unwrap()
        });

        let (status, stdout) = output.unwrap();
        assert!(status.success());
        assert_eq!(stdout, b"hi\n");
        assert!(matches!(&timed_out, Err(Error::CommandTimeout { command, .. }) if command == "sleep"),
            "{:?}", timed_out);
    }

    #[test]
    fn run_outside_a_runtime_is_error() {
        let result = CommandRunner::default().run(Path::new("true"), &[], limits(FINISH_LIMIT));

        assert!(matches!(&result, Err(Error::Io { path, .. }) if path == Path::new("true")), "{:?}", result);
    }

    #[test]
    fn timeout_kills_the_child() {
        let dir = TempDir::new();
        let pid_file = dir.0.join("pid");
        let _guard = script_lock();
        let script = write_script(&dir, "tool", &sleeping_body(&pid_file));
        let mut runner = CommandRunner::default();

        let start = Instant::now();
        let result = blocking(|| runner.run(&script, &[], limits(TIMEOUT_LIMIT)));
        let elapsed = start.elapsed();

        assert!(matches!(&result,
            Err(Error::CommandTimeout { command, seconds: 1 }) if Path::new(command) == script),
            "{:?}", result);
        assert!(elapsed < TIMEOUT_LIMIT + KILL_GRACE + RETURN_SLACK, "returned after {:?}", elapsed);
        // The killed child exits and is reaped, so the runner keeps nothing
        assert!(poll_until(|| runner.release_exited().then_some(())).is_some(), "the child is still kept");
        // A child scheduled late may be killed before it records its pid
        if let Some(pid) = written_pid(&pid_file) {
            assert!(wait_until_gone(&pid), "pid {} still exists", pid);
        }
    }

    fn run_script(body: &str) -> Result<(std::process::ExitStatus, Vec<u8>), Error> {
        let _guard = script_lock();
        blocking(|| CommandRunner::default().run(Path::new("sh"), &sh(body), limits(FINISH_LIMIT)))
    }

    #[test]
    fn output_of_exactly_the_cap_reads() {
        let (status, stdout) = run_script("printf '%04096d' 0").unwrap();

        assert!(status.success());
        assert_eq!(stdout.len() as u64, CAP);
    }

    #[test]
    fn output_one_byte_over_the_cap_is_error() {
        let result = run_script("printf '%04097d' 0");

        assert!(matches!(&result,
            Err(Error::Io { path, source })
                if path == Path::new("sh")
                    && source.kind() == io::ErrorKind::InvalidData
                    && source.to_string().contains("4096")),
            "{:?}", result);
    }

    #[test]
    fn failed_exit_status_is_returned_with_output() {
        let (status, stdout) = run_script("echo partial; exit 9").unwrap();

        assert_eq!(status.code(), Some(9));
        assert_eq!(stdout, b"partial\n");
    }

    #[test]
    fn unrepresentable_limit_is_error() {
        let result = blocking(|| CommandRunner::default()
            .run(Path::new("true"), &[], RunLimits { time: Duration::MAX, output: CAP }));

        assert!(matches!(&result,
            Err(Error::Io { path, source })
                if path == Path::new("true") && source.kind() == io::ErrorKind::InvalidInput),
            "{:?}", result);
    }

    /// The pipe inode that `fd` of process `pid` refers to, if it is a pipe
    fn pipe_of(pid: &str, fd: &str) -> Option<String> {
        let target = fs::read_link(Path::new("/proc").join(pid).join("fd").join(fd)).ok()?;
        let target = target.to_str()?;
        target.starts_with("pipe:").then(|| target.to_owned())
    }

    /// Whether this process holds a descriptor to `pipe`
    fn holds_pipe(pipe: &str) -> bool {
        fs::read_dir("/proc/self/fd").unwrap()
            .filter_map(|entry| fs::read_link(entry.ok()?.path()).ok())
            .any(|target| target.to_str() == Some(pipe))
    }

    #[test]
    fn descendant_holding_stdout_times_out_and_the_pipe_is_closed() {
        let dir = TempDir::new();
        let pid_file = dir.0.join("pid");
        let _stray = KillOnDrop(pid_file.clone());
        let _guard = script_lock();
        // The script exits at once, but the background sleep keeps its stdout open
        let script = write_script(&dir, "tool", &descendant_body(&pid_file));
        let mut runner = CommandRunner::default();

        let start = Instant::now();
        let result = blocking(|| runner.run(&script, &[], limits(READY_LIMIT)));
        let elapsed = start.elapsed();

        assert!(matches!(&result, Err(Error::CommandTimeout { .. })), "{:?}", result);
        assert!(elapsed < READY_LIMIT + KILL_GRACE + RETURN_SLACK, "returned after {:?}", elapsed);
        assert!(runner.release_exited(), "the exited script is still kept");
        let pid = written_pid(&pid_file).expect("the script did not record its descendant's pid");
        let pipe = pipe_of(&pid, "1").expect("stdout of the descendant is not a pipe");
        // Nothing of ours still reads the pipe the descendant writes to
        assert!(!holds_pipe(&pipe), "{} is still open here", pipe);
    }

    /// A script that creates `marker` when it runs
    fn marker_script(dir: &TempDir, marker: &Path) -> PathBuf {
        write_script(dir, "tool", &format!("touch '{}'", marker.display()))
    }

    #[test]
    fn child_alive_after_kill_grace_is_kept_and_blocks_the_next_run_until_it_exits() {
        let dir = TempDir::new();
        let marker = dir.0.join("ran");
        let _guard = script_lock();
        let script = marker_script(&dir, &marker);
        let mut runner = CommandRunner::default();

        blocking(|| {
            runner.keep(spawn_sleeper(), &script);

            let result = runner.run(&script, &[], limits(FINISH_LIMIT));

            assert!(matches!(&result,
                Err(Error::CommandStillRunning { command }) if *command == format!("{:?}", script)),
                "{:?}", result);
            assert!(!marker.exists(), "the program was started beside its kept child");

            runner.retained.as_mut().unwrap().start_kill().unwrap();
            let result = poll_until(|| match runner.run(&script, &[], limits(FINISH_LIMIT)) {
                Err(Error::CommandStillRunning { .. }) => None,
                other => Some(other),
            });

            let (status, _) = result.expect("the killed child is still kept").unwrap();
            assert!(status.success());
            assert!(marker.exists(), "the program did not run once its kept child exited");
        });
    }

    #[test]
    fn still_running_error_names_the_program_and_its_arguments() {
        let gpu = "00000000:81:00.0";
        let mut runner = CommandRunner::default();

        let result = blocking(move || {
            runner.keep(spawn_sleeper(), Path::new("nvidia-smi"));
            runner.run(Path::new("nvidia-smi"), &[OsStr::new("-i"), OsStr::new(gpu)], limits(FINISH_LIMIT))
        });

        let expected = r#""nvidia-smi" "-i" "00000000:81:00.0""#;
        assert!(matches!(&result, Err(Error::CommandStillRunning { command }) if command == expected),
            "{:?}", result);
        assert!(result.unwrap_err().to_string().ends_with(expected));
    }

    #[test]
    fn child_that_exited_after_kill_is_not_kept() {
        let dir = TempDir::new();
        let marker = dir.0.join("ran");
        let _guard = script_lock();
        let script = marker_script(&dir, &marker);
        let mut runner = CommandRunner::default();

        blocking(|| {
            let mut child = Command::new("true").kill_on_drop(true).spawn().unwrap();
            let status = Handle::current().block_on(child.wait());
            runner.after_kill(child, Some(status), &script);

            assert!(runner.retained.is_none());
            let (status, _) = runner.run(&script, &[], limits(FINISH_LIMIT)).unwrap();
            assert!(status.success());
            assert!(marker.exists());
        });
    }
}
