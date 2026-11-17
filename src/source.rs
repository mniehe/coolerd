use {
    std::{
        collections::{HashMap, HashSet},
        convert::TryInto,
        fs,
        io::{self, BufRead, BufReader, Read},
        path::{Path, PathBuf},
        process::{Child, ChildStdout, Command, ExitStatus, Stdio},
        sync::{
            mpsc::{self, Receiver, RecvTimeoutError},
            Arc,
            Mutex,
        },
        thread,
        time::{Duration, Instant},
    },
    log::warn,
    crate::{
        config::{Source, SourceConfig},
        error::{Error, Result},
        freeipmi::{SensorUnits, SensorValue},
        ipmi::Ipmi,
    },
};

/// Get the temperature of a hard drive via smartctl. This function fails if
/// smartctl does not return temperature data (eg. if a drive is in standby) or
/// if the reported temperature does not fit in a [`u8`].
fn parse_smart_source<T: AsRef<Path>>(block_dev: T) -> Result<u8> {
    let block_dev = block_dev.as_ref();

    let mut proc = Command::new("smartctl")
        .arg("-j")
        .arg("-A")
        .arg("-n")
        .arg("standby")
        .arg(block_dev)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Io { path: "(smartctl)".into(), source: e })?;

    let result = serde_json::from_reader(proc.stdout.take().unwrap());
    let status = proc.wait()
        .map_err(|e| Error::Io { path: "(smartctl)".into(), source: e })?;

    smart_reading(status, block_dev, result)
}

/// Turn smartctl's exit status and its parsed JSON output into a reading. A
/// failing exit status takes precedence over a JSON parse error.
fn smart_reading(
    status: ExitStatus,
    block_dev: &Path,
    result: serde_json::Result<serde_json::Value>,
) -> Result<u8> {
    match status.code() {
        // smartctl will return status code 2 when a drive is in standby
        Some(0) | Some(2) => {},
        _ => return Err(Error::Command { command: "smartctl".into(), status }),
    }

    let root = result
        .map_err(|e| Error::SmartParse { block_dev: block_dev.to_owned(), source: e })?;

    parse_smart_output(block_dev, &root)
}

/// Extract `temperature.current` from smartctl's JSON output
fn parse_smart_output(block_dev: &Path, root: &serde_json::Value) -> Result<u8> {
    let temperature = root
        .get("temperature")
        .and_then(|v| v.get("current"))
        .ok_or_else(|| Error::SmartNoReading(block_dev.to_owned()))?
        .as_u64()
        .and_then(|v| v.try_into().ok())
        .ok_or(Error::ReadingExceedsBounds)?;

    Ok(temperature)
}

fn parse_hdparm_output(block_dev: &Path, stdout: &mut dyn Read) -> Result<u8> {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();

    loop {
        let n = reader.read_line(&mut line)
            .map_err(|e| Error::Io { path: "(hdparm)".into(), source: e })?;
        if n == 0 {
            return Err(Error::HdparmNoData(block_dev.to_owned()));
        } else if line.contains("bad/missing sense data") {
            // hdparm exits with 0 when the drive responds with bad data
            return Err(Error::HdparmBadData(block_dev.to_owned()));
        } else if !line.contains("drive temperature (celsius) is:") {
            continue;
        }

        // The drive temperature line always has the number as the last token
        let last_token = line.trim_end().rsplit_once(' ')
            .ok_or_else(|| Error::HdparmBadData(block_dev.to_owned()))?
            .1;

        let temperature = last_token
            // Can be negative, but is within the bounds of 1 byte
            .parse::<i8>()
            .map_err(|e| Error::SensorValueParse { value: last_token.to_owned(), source: e })?
            .try_into()
            .map_err(|_| Error::ReadingExceedsBounds)?;

        return Ok(temperature);
    }
}

/// Get the temperature of a Hitachi/HGST/WD drive via hdparm. This function
/// fails if hdparm does not print the temperature line, hdparm prints the bad
/// sense data line, or if the reported temperature does not fit in a [`u8`].
fn parse_hdparm_source<T: AsRef<Path>>(block_dev: T) -> Result<u8> {
    let block_dev = block_dev.as_ref();

    let mut proc = Command::new("hdparm")
        .arg("-H")
        .arg(block_dev)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Io { path: "(hdparm)".into(), source: e })?;

    let result = parse_hdparm_output(block_dev, &mut proc.stdout.take().unwrap());
    let status = proc.wait()
        .map_err(|e| Error::Io { path: "(hdparm)".into(), source: e })?;

    if status.code() != Some(0) {
        return Err(Error::Command { command: "hdparm".into(), status });
    }

    result
}

/// Parse the output of `nvidia-smi --query-gpu=temperature.gpu
/// --format=csv,noheader,nounits` for one GPU. The output must be one record
/// of ASCII digits, the temperature in degrees Celsius, followed by at most
/// one line ending.
fn parse_nvidia_output(gpu: &str, stdout: &str) -> Result<u8> {
    let bad_data = || Error::NvidiaBadData { gpu: gpu.to_owned(), output: stdout.to_owned() };
    let record = stdout.strip_suffix("\r\n")
        .or_else(|| stdout.strip_suffix('\n'))
        .unwrap_or(stdout);

    if record.is_empty() || !record.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad_data());
    }

    let temperature = record
        .parse::<u32>()
        .map_err(|_| bad_data())?
        .try_into()
        .map_err(|_| Error::ReadingExceedsBounds)?;

    Ok(temperature)
}

/// Longest wait for one nvidia-smi run
const NVIDIA_SMI_TIMEOUT: Duration = Duration::from_secs(5);

/// How often [`run_with_timeout`] checks whether the child has exited
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Longest wait for a killed child to be reaped
const KILL_GRACE: Duration = Duration::from_millis(500);

/// Most bytes of a command's output that [`run_with_timeout`] reads
const OUTPUT_CAP: u64 = 4096;

/// Wait at most `grace` for `child` to exit and reap it. Returns `None` if the
/// child has not exited by then.
fn wait_bounded(child: &mut Child, grace: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + grace;

    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }

        let now = Instant::now();
        if now >= deadline {
            return None;
        }

        thread::sleep(CHILD_POLL_INTERVAL.min(deadline - now));
    }
}

/// Kill `child` and reap it within [`KILL_GRACE`]. A killed process can stay
/// in uninterruptible sleep, as nvidia-smi does in a hung driver, so a child
/// that outlives the grace is handed to a detached thread to reap and the
/// caller never waits on it.
fn stop_child(mut child: Child, program: &Path) {
    // Kill fails if the child has already exited, which is harmless
    let _ = child.kill();

    if wait_bounded(&mut child, KILL_GRACE).is_some() {
        return;
    }

    warn!("{:?} did not exit within {:?} of being killed and may be stuck; reaping it in the background",
        program, KILL_GRACE);

    let reaper = thread::Builder::new().spawn(move || {
        let _ = child.wait();
    });

    if let Err(e) = reaper {
        warn!("Failed to start a thread to reap {:?}: {}", program, e);
    }
}

/// Read `pipe` on a detached thread and send the result on the returned
/// channel. Output longer than [`OUTPUT_CAP`] bytes is an error. The thread
/// blocks for as long as any process holds the pipe open, which can outlast
/// the child, so the caller waits on the channel with a deadline.
fn spawn_output_reader(pipe: ChildStdout) -> io::Result<Receiver<io::Result<String>>> {
    let (sender, receiver) = mpsc::channel();

    thread::Builder::new().spawn(move || {
        // The receiver is gone once the caller has timed out
        let _ = sender.send(read_capped(pipe));
    })?;

    Ok(receiver)
}

/// Read all of `pipe`, failing once it yields more than [`OUTPUT_CAP`] bytes.
/// One byte past the cap is read so that a cut output, whose prefix may still
/// parse, is never returned.
fn read_capped(pipe: impl Read) -> io::Result<String> {
    let mut output = Vec::new();
    pipe.take(OUTPUT_CAP + 1).read_to_end(&mut output)?;

    if output.len() as u64 > OUTPUT_CAP {
        return Err(io::Error::new(io::ErrorKind::InvalidData,
            format!("output is longer than {} bytes", OUTPUT_CAP)));
    }

    String::from_utf8(output).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Run a command with piped stdout and return its exit status and its output,
/// which must be at most [`OUTPUT_CAP`] bytes. If the command has not exited
/// and closed its output within `limit`, [`Error::CommandTimeout`] is
/// returned; a child still running then is killed. The call returns within
/// about `limit` plus [`KILL_GRACE`] in every case.
fn run_with_timeout(cmd: &mut Command, limit: Duration) -> Result<(ExitStatus, String)> {
    let program = PathBuf::from(cmd.get_program());
    let io_error = |e| Error::Io { path: program.clone(), source: e };
    let timeout = || Error::CommandTimeout {
        command: program.display().to_string(),
        seconds: limit.as_secs(),
    };
    let deadline = Instant::now()
        .checked_add(limit)
        .ok_or_else(|| io_error(io::Error::new(io::ErrorKind::InvalidInput,
            format!("time limit {:?} is too long", limit))))?;

    let mut child = cmd
        .stdout(Stdio::piped())
        .spawn()
        .map_err(io_error)?;

    let reader = child.stdout.take()
        .ok_or_else(|| io::Error::other("stdout is not piped"))
        .and_then(spawn_output_reader);
    let output = match reader {
        Ok(receiver) => receiver,
        Err(e) => {
            stop_child(child, &program);
            return Err(io_error(e));
        }
    };

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = receive_output(&output, deadline)
                    .ok_or_else(timeout)?
                    .map_err(io_error)?;
                return Ok((status, stdout));
            }
            Ok(None) => {},
            Err(e) => {
                stop_child(child, &program);
                return Err(io_error(e));
            }
        }

        let now = Instant::now();
        if now >= deadline {
            stop_child(child, &program);
            return Err(timeout());
        }

        thread::sleep(CHILD_POLL_INTERVAL.min(deadline - now));
    }
}

/// Wait until `deadline` for the output reader's result. Returns `None` if
/// the deadline passes first, which happens when a descendant of the child
/// still holds the pipe open.
fn receive_output(output: &Receiver<io::Result<String>>, deadline: Instant)
    -> Option<io::Result<String>>
{
    match output.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(result) => Some(result),
        Err(RecvTimeoutError::Timeout) => None,
        Err(RecvTimeoutError::Disconnected) =>
            Some(Err(io::Error::other("output reader stopped without a result"))),
    }
}

/// Get the temperature of one NVIDIA GPU via nvidia-smi, waiting at most
/// `limit` for nvidia-smi to exit. `gpu` is a PCI bus id or a GPU UUID.
fn read_nvidia_with_limit(program: &Path, gpu: &str, limit: Duration) -> Result<u8> {
    let mut cmd = Command::new(program);
    cmd.arg("-i")
        .arg(gpu)
        .arg("--query-gpu=temperature.gpu")
        .arg("--format=csv,noheader,nounits");

    let (status, stdout) = run_with_timeout(&mut cmd, limit)?;

    if status.code() != Some(0) {
        return Err(Error::Command { command: program.to_owned(), status });
    }

    parse_nvidia_output(gpu, &stdout)
}

/// Get the temperature of one NVIDIA GPU via nvidia-smi. This function fails
/// if nvidia-smi exits with a non-zero status, does not exit within
/// [`NVIDIA_SMI_TIMEOUT`], or does not print a temperature that fits in a
/// [`u8`].
fn read_nvidia_source(program: &Path, gpu: &str) -> Result<u8> {
    read_nvidia_with_limit(program, gpu, NVIDIA_SMI_TIMEOUT)
}

/// Get the temperature from a plain-text file (typically a sysfs path). The
/// contents of the file should be a decimal-formatted integer in units of
/// thousandths degrees Celsius after whitespace is trimmed. If the temperature,
/// after being converted to degrees Celsius, does not fit in a [`u8`], then
/// [`Error::ReadingExceedsBounds`] is returned.
fn parse_file_source<T: AsRef<Path>>(path: T) -> Result<u8> {
    let contents = fs::read_to_string(path.as_ref())
        .map_err(|e| Error::Io { path: path.as_ref().to_owned(), source: e })?;
    let trimmed = contents.trim();

    // The file should be in milli-degrees Celsius
    let temperature = trimmed
        .parse::<u32>()
        .map_err(|e| Error::SensorValueParse { value: trimmed.to_owned(), source: e })?
        .checked_div(1000)
        .and_then(|t| t.try_into().ok())
        .ok_or(Error::ReadingExceedsBounds)?;

    Ok(temperature)
}

/// Get the temperatures for the given list of sensors from IPMI. This queries
/// all temperature sensors and then filters the results. This function only
/// fails if the IPMI sensor query fails. If a sensor's unit is not degrees
/// Celsius or if the value exceeds the bounds of a `u8`, then the reported
/// value of that sensor will be `None`.
fn parse_ipmi_sources(ipmi: Arc<Mutex<Ipmi>>, sensors: &HashSet<String>)
    -> Result<HashMap<String, u8>>
{
    if sensors.is_empty() {
        return Ok(HashMap::default());
    }

    let mut ipmi_lock = ipmi.lock().unwrap();
    let ipmi_readings = ipmi_lock.get_temperature_readings()?;
    let mut result = HashMap::new();

    for sensor in sensors {
        let reading = match ipmi_readings.get(sensor) {
            Some(r) => r,
            None => return Err(Error::SensorNotFound(sensor.into())),
        };

        let reading = match reading {
            Some(r) => r,
            None => return Err(Error::SensorNoReading(sensor.into())),
        };

        if reading.units != SensorUnits::Celsius {
            return Err(Error::SensorBadUnits {
                sensor: sensor.into(),
                units: reading.units,
            });
        }

        let temperature = match reading.value {
            SensorValue::Uint32(t) => t.try_into().ok(),
            SensorValue::Double(t) => (t as u32).try_into().ok(),
            v => return Err(Error::SensorBadValue {
                sensor: sensor.into(),
                value: v,
            }),
        }.ok_or(Error::ReadingExceedsBounds)?;

        result.insert(sensor.into(), temperature);
    }

    Ok(result)
}

/// Get temperature readings for the given sources. The returned values are in
/// the same order as given.
pub fn get_source_readings(ipmi: Arc<Mutex<Ipmi>>, sources: &[SourceConfig])
    -> Result<Vec<u8>>
{
    // Get IPMI sensor readings in one go for better performance.
    let ipmi_sensors = sources.iter()
        .filter_map(|s| {
            match &s.kind {
                Source::Ipmi { sensor } => Some(sensor.clone()),
                _ => None,
            }
        })
        .collect::<HashSet<_>>();

    let ipmi_results = parse_ipmi_sources(ipmi, &ipmi_sensors)?;

    sources.iter()
        .map(|s| {
            match &s.kind {
                Source::Ipmi { sensor } => Ok(ipmi_results[sensor.as_str()]),
                Source::File { path } => parse_file_source(path),
                Source::Smart { block_dev } => parse_smart_source(block_dev),
                Source::Hdparm { block_dev } => parse_hdparm_source(block_dev),
                Source::Nvidia { gpu } => read_nvidia_source(Path::new("nvidia-smi"), gpu),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use {
        std::{
            env,
            fs,
            io,
            os::unix::{fs::PermissionsExt, process::ExitStatusExt},
            path::{Path, PathBuf},
            process::{self, Child, Command, ExitStatus, Stdio},
            sync::{
                atomic::{AtomicUsize, Ordering},
                Mutex,
                MutexGuard,
            },
            thread,
            time::{Duration, Instant},
        },
        serde_json::json,
        super::{
            KILL_GRACE,
            OUTPUT_CAP,
            parse_file_source,
            parse_hdparm_output,
            parse_nvidia_output,
            parse_smart_output,
            read_nvidia_with_limit,
            run_with_timeout,
            smart_reading,
            wait_bounded,
        },
        crate::error::Error,
    };

    const BLOCK_DEV: &str = "/dev/sda";
    const GPU: &str = "00000000:81:00.0";
    const SMART_OUTPUT: &str = r#"{ "temperature": { "current": 41 } }"#;
    const MALFORMED_SMART_OUTPUT: &str = r#"{ "temperature": "#;

    /// Exit status of a process that exited with `code`
    fn exit_status(code: i32) -> ExitStatus {
        // A raw wait status holds the exit code in its second byte
        ExitStatus::from_raw(code << 8)
    }

    fn smart_status_reading(code: i32, output: &str) -> Result<u8, Error> {
        smart_reading(exit_status(code), Path::new(BLOCK_DEV), serde_json::from_str(output))
    }

    /// Directory under the system temp dir that is removed when dropped
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);

            let name = format!("ipmi-fan-control-test-{}-{}",
                process::id(), COUNTER.fetch_add(1, Ordering::SeqCst));
            let path = env::temp_dir().join(name);
            fs::create_dir(&path).unwrap();

            Self(path)
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
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

    fn file_reading(contents: &str) -> Result<u8, Error> {
        let dir = TempDir::new();
        let path = dir.write("temp", contents);
        parse_file_source(path)
    }

    fn hdparm_reading(output: &str) -> Result<u8, Error> {
        parse_hdparm_output(Path::new(BLOCK_DEV), &mut output.as_bytes())
    }

    #[test]
    fn smart_current_temperature() {
        let root = json!({ "temperature": { "current": 41 } });

        assert_eq!(parse_smart_output(Path::new(BLOCK_DEV), &root).unwrap(), 41);
    }

    #[test]
    fn smart_without_temperature_has_no_reading() {
        let root = json!({ "smart_status": { "passed": true } });
        let result = parse_smart_output(Path::new(BLOCK_DEV), &root);

        assert!(matches!(result, Err(Error::SmartNoReading(p)) if p == Path::new(BLOCK_DEV)));
    }

    #[test]
    fn smart_temperature_above_u8_exceeds_bounds() {
        let root = json!({ "temperature": { "current": 256 } });
        let result = parse_smart_output(Path::new(BLOCK_DEV), &root);

        assert!(matches!(result, Err(Error::ReadingExceedsBounds)));
    }

    #[test]
    fn smart_failed_status_wins_over_malformed_json() {
        let result = smart_status_reading(1, MALFORMED_SMART_OUTPUT);

        assert!(matches!(&result,
            Err(Error::Command { command, status })
                if command == Path::new("smartctl") && status.code() == Some(1)),
            "{:?}", result);
    }

    #[test]
    fn smart_standby_status_with_malformed_json_fails_to_parse() {
        let result = smart_status_reading(2, MALFORMED_SMART_OUTPUT);

        assert!(matches!(&result,
            Err(Error::SmartParse { block_dev, .. }) if block_dev == Path::new(BLOCK_DEV)),
            "{:?}", result);
    }

    #[test]
    fn smart_success_status_gives_reading() {
        assert_eq!(smart_status_reading(0, SMART_OUTPUT).unwrap(), 41);
    }

    #[test]
    fn smart_standby_status_with_valid_json_gives_reading() {
        assert_eq!(smart_status_reading(2, SMART_OUTPUT).unwrap(), 41);
    }

    #[test]
    fn hdparm_temperature_line() {
        let output = "\n/dev/sda:\n drive temperature (celsius) is:  33\n drive temperature in range:  yes\n";

        assert_eq!(hdparm_reading(output).unwrap(), 33);
    }

    #[test]
    fn hdparm_negative_temperature_exceeds_bounds() {
        let output = "\n/dev/sda:\n drive temperature (celsius) is:  -5\n";

        assert!(matches!(hdparm_reading(output), Err(Error::ReadingExceedsBounds)));
    }

    #[test]
    fn hdparm_bad_sense_data() {
        let output = "\n/dev/sda:\nSG_IO: bad/missing sense data, sb[]:  70 00 05 00\n";

        assert!(matches!(hdparm_reading(output),
            Err(Error::HdparmBadData(p)) if p == Path::new(BLOCK_DEV)));
    }

    #[test]
    fn hdparm_without_temperature_has_no_data() {
        let output = "\n/dev/sda:\n";

        assert!(matches!(hdparm_reading(output),
            Err(Error::HdparmNoData(p)) if p == Path::new(BLOCK_DEV)));
    }

    #[test]
    fn file_millidegrees() {
        assert_eq!(file_reading("42000\n").unwrap(), 42);
    }

    #[test]
    fn file_non_numeric_fails_to_parse() {
        assert!(matches!(file_reading("hot\n"),
            Err(Error::SensorValueParse { value, .. }) if value == "hot"));
    }

    #[test]
    fn file_highest_u8_temperature() {
        assert_eq!(file_reading("255999").unwrap(), 255);
    }

    #[test]
    fn file_temperature_above_u8_exceeds_bounds() {
        assert!(matches!(file_reading("256000"), Err(Error::ReadingExceedsBounds)));
    }

    fn assert_nvidia_bad_data(stdout: &str) {
        let result = parse_nvidia_output(GPU, stdout);

        assert!(matches!(&result,
            Err(Error::NvidiaBadData { gpu, output }) if gpu == GPU && output == stdout),
            "{:?}", result);
    }

    #[test]
    fn nvidia_temperature_line() {
        assert_eq!(parse_nvidia_output(GPU, "67\n").unwrap(), 67);
    }

    #[test]
    fn nvidia_not_available_is_bad_data() {
        assert_nvidia_bad_data("[N/A]\n");
    }

    #[test]
    fn nvidia_empty_output_is_bad_data() {
        assert_nvidia_bad_data("");
    }

    #[test]
    fn nvidia_two_lines_are_bad_data() {
        assert_nvidia_bad_data("67\n68\n");
    }

    #[test]
    fn nvidia_bad_data_message_names_gpu_and_trimmed_output() {
        let message = parse_nvidia_output(GPU, "[N/A]\n").unwrap_err().to_string();

        assert_eq!(message, r#"nvidia-smi reported bad data for GPU: 00000000:81:00.0: "[N/A]""#);
    }

    #[test]
    fn nvidia_record_with_or_without_one_line_ending_gives_reading() {
        for stdout in ["67", "67\n", "67\r\n"] {
            assert_eq!(parse_nvidia_output(GPU, stdout).unwrap(), 67, "{:?}", stdout);
        }
    }

    #[test]
    fn nvidia_blank_lines_are_bad_data() {
        assert_nvidia_bad_data("\n67\n");
        assert_nvidia_bad_data("67\n\n");
    }

    #[test]
    fn nvidia_sign_is_bad_data() {
        assert_nvidia_bad_data("+0\n");
    }

    #[test]
    fn nvidia_spaces_around_record_are_bad_data() {
        assert_nvidia_bad_data(" 67\n");
        assert_nvidia_bad_data("67 \n");
    }

    #[test]
    fn nvidia_temperature_above_u8_exceeds_bounds() {
        let result = parse_nvidia_output(GPU, "300\n");

        assert!(matches!(result, Err(Error::ReadingExceedsBounds)), "{:?}", result);
    }

    /// Limit for a script run that must finish, far above any scheduling delay
    const FINISH_LIMIT: Duration = Duration::from_secs(10);

    /// Limit for a script run that must time out. It is long enough for the
    /// script to write its pid file before the limit passes.
    const TIMEOUT_LIMIT: Duration = Duration::from_secs(1);

    /// Grace for the tests of `wait_bounded` on a child that does not exit
    const SHORT_GRACE: Duration = Duration::from_millis(200);

    /// Longest a test waits for a script's file or a child's state to appear
    const APPEAR_WAIT: Duration = Duration::from_secs(5);

    /// Time a bounded call may take beyond its bound on a loaded host
    const RETURN_SLACK: Duration = Duration::from_secs(2);

    // Serialises the tests that spawn a process. A fork in another test
    // thread can inherit a script's write descriptor for a moment, and exec
    // of the script then fails with ETXTBSY.
    static SCRIPT_LOCK: Mutex<()> = Mutex::new(());

    fn script_lock() -> MutexGuard<'static, ()> {
        SCRIPT_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Poll `ready` until it gives a value or [`APPEAR_WAIT`] passes
    fn poll_until<T>(mut ready: impl FnMut() -> Option<T>) -> Option<T> {
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

    /// Wait for a script to write a pid to `path` and return it
    fn wait_for_pid(path: &Path) -> Option<String> {
        poll_until(|| {
            let contents = fs::read_to_string(path).ok()?;
            let pid = contents.trim();
            (!pid.is_empty()).then(|| pid.to_owned())
        })
    }

    /// Wait until the process `pid` has no /proc entry. A zombie still has
    /// one, so this also waits for the process to be reaped.
    fn wait_until_gone(pid: &str) -> bool {
        let entry = Path::new("/proc").join(pid);
        poll_until(|| (!entry.exists()).then_some(())).is_some()
    }

    /// Kills the process whose pid a script wrote to a file, when dropped. A
    /// script that never wrote the file leaves nothing to kill.
    struct KillOnDrop(PathBuf);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let Some(pid) = wait_for_pid(&self.0) else {
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

    /// Owns a child and kills and reaps it when dropped, so a failed
    /// assertion leaves no process behind
    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            // Both calls do nothing to a child that has already been reaped
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Spawn a child that runs until killed
    fn spawn_sleeper() -> ChildGuard {
        let child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        ChildGuard(child)
    }

    #[test]
    fn wait_bounded_gives_up_on_live_child() {
        let _guard = script_lock();
        let mut child = spawn_sleeper();

        let start = Instant::now();
        let status = wait_bounded(&mut child.0, SHORT_GRACE);
        let elapsed = start.elapsed();

        assert!(status.is_none(), "{:?}", status);
        assert!(elapsed >= SHORT_GRACE, "returned after {:?}", elapsed);
        assert!(elapsed < SHORT_GRACE + RETURN_SLACK, "returned after {:?}", elapsed);
    }

    #[test]
    fn wait_bounded_returns_once_child_exits() {
        let _guard = script_lock();
        let mut child = ChildGuard(Command::new("sh")
            .args(["-c", "exit 4"])
            .spawn()
            .unwrap());

        let grace = Duration::from_secs(10);
        let start = Instant::now();
        let status = wait_bounded(&mut child.0, grace);
        let elapsed = start.elapsed();

        assert_eq!(status.and_then(|s| s.code()), Some(4));
        assert!(elapsed < grace / 2, "returned after {:?}", elapsed);
    }

    /// A fake nvidia-smi script and the outcome of reading it
    struct ScriptRun {
        script: PathBuf,
        result: Result<u8, Error>,
        /// Time of the read alone, without the wait for [`SCRIPT_LOCK`]
        elapsed: Duration,
    }

    /// Run `body` as a fake nvidia-smi script in `dir`
    fn nvidia_script_reading(dir: &TempDir, body: &str, limit: Duration) -> ScriptRun {
        let _guard = script_lock();
        let script = dir.write("nvidia-smi", &format!("#!/bin/sh\n{}\n", body));
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        let start = Instant::now();
        let result = read_nvidia_with_limit(&script, GPU, limit);
        ScriptRun { script, result, elapsed: start.elapsed() }
    }

    fn assert_timed_out(run: &ScriptRun) {
        assert!(matches!(&run.result,
            Err(Error::CommandTimeout { command, .. }) if Path::new(command) == run.script),
            "{:?}", run.result);
    }

    #[test]
    fn nvidia_script_output_gives_reading() {
        let dir = TempDir::new();
        let body = format!(
            "[ \"$*\" = \"-i {} --query-gpu=temperature.gpu --format=csv,noheader,nounits\" ] || exit 3\necho 71",
            GPU);
        let run = nvidia_script_reading(&dir, &body, FINISH_LIMIT);

        assert_eq!(run.result.unwrap(), 71);
    }

    fn assert_output_over_cap(run: &ScriptRun) {
        assert!(matches!(&run.result,
            Err(Error::Io { path, source })
                if *path == run.script
                    && source.kind() == io::ErrorKind::InvalidData
                    && source.to_string().contains(&OUTPUT_CAP.to_string())),
            "{:?}", run.result);
    }

    #[test]
    fn nvidia_script_digits_past_output_cap_are_error() {
        let dir = TempDir::new();
        // 4096 zero-padded digits ending in 1, then a 7: the full record is 17
        let run = nvidia_script_reading(&dir, "printf '%04096d7\\n' 1", FINISH_LIMIT);

        assert_output_over_cap(&run);
    }

    #[test]
    fn nvidia_script_record_after_output_cap_is_error() {
        let dir = TempDir::new();
        // The first record ends exactly at the cap and a second one follows
        let run = nvidia_script_reading(&dir, "printf '%04095d\\n72\\n' 71", FINISH_LIMIT);

        assert_output_over_cap(&run);
    }

    #[test]
    fn nvidia_script_output_of_exactly_cap_gives_reading() {
        let dir = TempDir::new();
        let run = nvidia_script_reading(&dir, "printf '%04095d\\n' 71", FINISH_LIMIT);

        assert_eq!(run.result.unwrap(), 71);
    }

    #[test]
    fn unrepresentable_limit_is_error() {
        let _guard = script_lock();
        let result = run_with_timeout(&mut Command::new("true"), Duration::MAX);

        assert!(matches!(&result,
            Err(Error::Io { path, source })
                if path == Path::new("true") && source.kind() == io::ErrorKind::InvalidInput),
            "{:?}", result);
    }

    #[test]
    fn nvidia_script_failure_is_command_error() {
        let dir = TempDir::new();
        let run = nvidia_script_reading(&dir, "exit 9", FINISH_LIMIT);

        assert!(matches!(&run.result,
            Err(Error::Command { command, status })
                if *command == run.script && status.code() == Some(9)),
            "{:?}", run.result);
    }

    #[test]
    fn nvidia_script_descendant_holding_output_times_out() {
        let dir = TempDir::new();
        let pid_file = dir.0.join("pid");
        let _stray = KillOnDrop(pid_file.clone());
        // The script exits at once, but the background sleep keeps its stdout open
        let body = format!("sleep 30 &\necho $! > '{}'\necho 71", pid_file.display());

        let run = nvidia_script_reading(&dir, &body, TIMEOUT_LIMIT);

        assert_timed_out(&run);
        assert!(run.elapsed < TIMEOUT_LIMIT + RETURN_SLACK, "returned after {:?}", run.elapsed);
    }

    #[test]
    fn nvidia_script_timeout_kills_child() {
        let dir = TempDir::new();
        let pid_file = dir.0.join("pid");
        // exec keeps the shell's pid, so the recorded pid is the sleeping process
        let body = format!("echo $$ > '{}'\nexec sleep 30", pid_file.display());

        let run = nvidia_script_reading(&dir, &body, TIMEOUT_LIMIT);

        assert_timed_out(&run);
        assert!(run.elapsed < TIMEOUT_LIMIT + KILL_GRACE + RETURN_SLACK,
            "returned after {:?}", run.elapsed);

        // A child that outlived the kill grace is reaped by a detached thread,
        // so its /proc entry may take a moment longer to go away
        let pid = wait_for_pid(&pid_file).expect("the script wrote no pid before it was killed");
        assert!(wait_until_gone(&pid), "pid {} still exists", pid);
    }
}
