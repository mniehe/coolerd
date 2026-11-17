use {
    std::{
        collections::HashMap,
        convert::TryInto,
        fs,
        io::{self, BufRead, BufReader, Read},
        iter,
        path::{Path, PathBuf},
        process::{Child, ChildStdout, Command, ExitStatus, Stdio},
        sync::mpsc::{self, Receiver, RecvTimeoutError},
        thread,
        time::{Duration, Instant},
    },
    log::{trace, warn},
    crate::{
        config::{OnFailure, Source, SourceConfig, Zone},
        curve::SourceReading,
        error::{Error, Result},
        freeipmi::{SensorReading, SensorUnits, SensorValue},
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

/// Get the temperature of one IPMI sensor from the readings of every
/// temperature sensor. This function fails if the sensor is missing or has no
/// reading, if its unit is not degrees Celsius, or if its value is not a
/// number from 0 to 255. A fractional value is truncated.
fn ipmi_sensor_temp(sensor: &str, readings: &HashMap<String, Option<SensorReading>>) -> Result<u8> {
    let reading = match readings.get(sensor) {
        Some(Some(r)) => r,
        Some(None) => return Err(Error::SensorNoReading(sensor.into())),
        None => return Err(Error::SensorNotFound(sensor.into())),
    };

    if reading.units != SensorUnits::Celsius {
        return Err(Error::SensorBadUnits {
            sensor: sensor.into(),
            units: reading.units,
        });
    }

    let temperature = match reading.value {
        SensorValue::Uint32(t) => t.try_into().ok(),
        SensorValue::Double(t) => double_to_temp(t),
        v => return Err(Error::SensorBadValue {
            sensor: sensor.into(),
            value: v,
        }),
    };

    temperature.ok_or_else(|| Error::SensorValueOutOfRange {
        sensor: sensor.into(),
        value: reading.value,
    })
}

/// Truncate a sensor's floating-point value to a temperature. Returns `None`
/// for a value that is not a number, is negative, or is 256 or more, which a
/// cast would turn into 0 or 255 and pass as a reading.
fn double_to_temp(value: f64) -> Option<u8> {
    if !(0.0..256.0).contains(&value) {
        return None;
    }

    Some(value as u8)
}

/// Read one source that is not an IPMI sensor
pub fn read_source(source: &Source) -> Result<u8> {
    match source {
        // read_zone_sources reads IPMI sensors in one batch and never passes
        // one here
        Source::Ipmi { sensor } => Err(Error::SensorNotFound(sensor.clone())),
        Source::File { path } => parse_file_source(path),
        Source::Smart { block_dev } => parse_smart_source(block_dev),
        Source::Hdparm { block_dev } => parse_hdparm_source(block_dev),
        Source::Nvidia { gpu } => read_nvidia_source(Path::new("nvidia-smi"), gpu),
    }
}

/// The waits left to one poll, shared by all sources of a zone
type Waits<'a> = dyn Iterator<Item = Duration> + 'a;

/// The reading of a source that failed its last attempt with `error` after
/// `attempts` attempts. An `exit` source returns the error.
fn exhausted(on_failure: &OnFailure, attempts: u64, error: Error) -> Result<SourceReading> {
    let error = Error::RetriesFailed { attempts, source: Box::new(error) };

    match on_failure {
        OnFailure::Exit {} => Err(error),
        OnFailure::Ignore {} | OnFailure::Dcycle { .. } => Ok(SourceReading::Failed(error.to_string())),
    }
}

/// Whether the source at `index` failed in the last poll, by the flags
/// [`read_zone_sources`] was given. A source without a flag gets its retries.
fn failed_last(flags: &[bool], index: usize) -> bool {
    flags.get(index).copied().unwrap_or(false)
}

/// An IPMI source that has no reading yet in this poll
struct PendingSensor<'a> {
    index: usize,
    sensor: &'a str,
    on_failure: &'a OnFailure,
    /// A sensor that failed in the last poll causes no retry of its own
    failed_last: bool,
}

/// Take the wait before the next query, unless every pending sensor failed in
/// the last poll
fn retry_wait<'a>(mut pending: impl Iterator<Item = &'a PendingSensor<'a>>, waits: &mut Waits) -> Option<Duration> {
    if !pending.any(|p| !p.failed_last) {
        return None;
    }

    waits.next()
}

/// Read every IPMI source of `zone` into `readings`, by index, with one
/// `query_ipmi` call per attempt. Each attempt after the first takes a wait
/// from `waits` and covers only the sources that failed the attempt before.
/// A sensor that failed in the last poll still takes the value of a later
/// attempt, but no attempt is made for its sake alone.
fn read_ipmi_sources(
    zone: &Zone,
    failed_flags: &[bool],
    query_ipmi: &mut dyn FnMut() -> Result<HashMap<String, Option<SensorReading>>>,
    waits: &mut Waits,
    readings: &mut [Option<SourceReading>],
) -> Result<()> {
    let mut pending: Vec<PendingSensor> = zone.sources.iter()
        .enumerate()
        .filter_map(|(index, source)| match &source.kind {
            Source::Ipmi { sensor } => Some(PendingSensor {
                index,
                sensor,
                on_failure: &source.on_failure,
                failed_last: failed_last(failed_flags, index),
            }),
            _ => None,
        })
        .collect();
    let mut attempts = 0;

    while !pending.is_empty() {
        attempts += 1;

        let sensor_readings = match query_ipmi() {
            Ok(r) => r,
            Err(e) => {
                let Some(delay) = retry_wait(pending.iter(), waits) else {
                    return exhaust_query(&pending, attempts, e, readings);
                };
                trace!("IPMI sensor query failed on attempt {}: {}", attempts, e);
                thread::sleep(delay);
                continue;
            }
        };

        let mut failures = Vec::new();

        for p in pending {
            match ipmi_sensor_temp(p.sensor, &sensor_readings) {
                Ok(temp) => readings[p.index] = Some(SourceReading::Temp(temp)),
                Err(e) => failures.push((p, e)),
            }
        }

        if failures.is_empty() {
            break;
        }

        let Some(delay) = retry_wait(failures.iter().map(|(p, _)| p), waits) else {
            for (p, e) in failures {
                readings[p.index] = Some(exhausted(p.on_failure, attempts, e)?);
            }
            break;
        };

        for (p, e) in &failures {
            trace!("IPMI sensor {:?} failed on attempt {}: {}", p.sensor, attempts, e);
        }
        thread::sleep(delay);

        pending = failures.into_iter().map(|(p, _)| p).collect();
    }

    Ok(())
}

/// Settle the IPMI sources still pending when the sensor query itself failed
/// its last attempt. [`Error`] is not `Clone`, so the error goes to the first
/// `exit` source, which stops the reading, and the rest get its text.
fn exhaust_query(
    pending: &[PendingSensor],
    attempts: u64,
    error: Error,
    readings: &mut [Option<SourceReading>],
) -> Result<()> {
    let error = Error::RetriesFailed { attempts, source: Box::new(error) };

    if pending.iter().any(|p| *p.on_failure == OnFailure::Exit {}) {
        return Err(error);
    }

    for p in pending {
        readings[p.index] = Some(SourceReading::Failed(error.to_string()));
    }

    Ok(())
}

/// Read one source with `reader`, taking a wait from `waits` before each retry
fn read_with_retries(
    source: &SourceConfig,
    reader: &dyn Fn(&Source) -> Result<u8>,
    waits: &mut Waits,
) -> Result<SourceReading> {
    let mut attempts = 0;

    loop {
        attempts += 1;

        let error = match reader(&source.kind) {
            Ok(temp) => return Ok(SourceReading::Temp(temp)),
            Err(e) => e,
        };

        let Some(delay) = waits.next() else {
            return exhausted(&source.on_failure, attempts, error);
        };

        trace!("Source {:?} failed on attempt {}: {}", source.kind, attempts, error);
        thread::sleep(delay);
    }
}

/// Read every source of `zone` once, retrying only a source that failed.
/// `reader` reads each source that is not an IPMI sensor, in source order;
/// then the IPMI sensors are read last, as one `query_ipmi` call per attempt,
/// so that their readings are fresh when the duty is computed.
///
/// All retries of the call share the zone's `retries` waits of
/// `retry_delay_ms`. A source marked in `failed_last`, which failed in the
/// last poll, gets one attempt and takes no wait, so a source that keeps
/// failing leaves the waits to the others.
///
/// A source that fails when no wait is left gives a failed reading, unless its
/// `on_failure` is `exit`: then its error is returned at once and nothing more
/// is read or queried.
pub fn read_zone_sources(
    zone: &Zone,
    failed_flags: &[bool],
    query_ipmi: &mut dyn FnMut() -> Result<HashMap<String, Option<SensorReading>>>,
    reader: &dyn Fn(&Source) -> Result<u8>,
) -> Result<Vec<SourceReading>> {
    let mut waits = zone.retry_iter();
    let mut readings = vec![None; zone.sources.len()];

    for (index, source) in zone.sources.iter().enumerate() {
        if matches!(source.kind, Source::Ipmi { .. }) {
            continue;
        }

        let mut no_waits = iter::empty();
        let source_waits: &mut Waits = if failed_last(failed_flags, index) {
            &mut no_waits
        } else {
            &mut waits
        };

        readings[index] = Some(read_with_retries(source, reader, source_waits)?);
    }

    read_ipmi_sources(zone, failed_flags, query_ipmi, &mut waits, &mut readings)?;

    // Every source has a reading by now. A gap would shorten the list, and
    // ZoneState answers a short list with full duty.
    Ok(readings.into_iter().flatten().collect())
}

#[cfg(test)]
mod tests {
    use {
        std::{
            cell::RefCell,
            collections::HashMap,
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
            ipmi_sensor_temp,
            parse_file_source,
            parse_hdparm_output,
            parse_nvidia_output,
            parse_smart_output,
            read_nvidia_with_limit,
            read_zone_sources,
            run_with_timeout,
            smart_reading,
            wait_bounded,
        },
        crate::{
            config::{
                Aggregation,
                Interval,
                OnFailure,
                Retries,
                RetryDelayMs,
                SessionName,
                Source,
                SourceConfig,
                Step,
                Zone,
            },
            curve::{
                SourceReading::{self, Failed, Temp},
                ZoneState,
            },
            error::Error,
            freeipmi::{SensorReading, SensorUnits, SensorValue},
        },
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

    /// A zone with no wait between retries
    fn zone(sources: Vec<SourceConfig>, retries: usize) -> Zone {
        Zone {
            session: SessionName::default(),
            interval: Interval::default(),
            retries: Retries(retries),
            retry_delay_ms: RetryDelayMs(0),
            ipmi_zones: vec![0],
            sources,
            aggregation: Aggregation::Maximum,
            steps: Some(vec![Step { temp: 45, dcycle: 45 }, Step { temp: 75, dcycle: 100 }]),
            duty_aggregation: Aggregation::Maximum,
            hysteresis: 0,
        }
    }

    fn file(path: &str, on_failure: OnFailure) -> SourceConfig {
        SourceConfig { kind: Source::File { path: path.to_owned() }, steps: None, on_failure }
    }

    fn ipmi(sensor: &str, on_failure: OnFailure) -> SourceConfig {
        SourceConfig { kind: Source::Ipmi { sensor: sensor.to_owned() }, steps: None, on_failure }
    }

    const IGNORE: OnFailure = OnFailure::Ignore {};
    const CPU: &str = "CPU Temp";

    /// A fake reader of file sources. Each path gives its outcomes in turn,
    /// `None` for a failure, and repeats its last outcome; every call is
    /// recorded.
    struct FakeReader {
        paths: Vec<(&'static str, Vec<Option<u8>>)>,
        calls: RefCell<Vec<String>>,
    }

    impl FakeReader {
        /// Each path fails a set number of times and then reads as its
        /// temperature: path, failures before the first good read, temperature
        fn new(paths: &[(&'static str, usize, u8)]) -> Self {
            let paths = paths.iter()
                .map(|&(path, failures, temp)| {
                    let mut outcomes = vec![None; failures];
                    outcomes.push(Some(temp));
                    (path, outcomes)
                })
                .collect();

            Self { paths, calls: RefCell::new(Vec::new()) }
        }

        fn scripted(paths: &[(&'static str, &[Option<u8>])]) -> Self {
            let paths = paths.iter().map(|&(path, outcomes)| (path, outcomes.to_vec())).collect();

            Self { paths, calls: RefCell::new(Vec::new()) }
        }

        fn read(&self, source: &Source) -> Result<u8, Error> {
            let Source::File { path } = source else {
                panic!("the reader was given {:?}", source);
            };
            self.calls.borrow_mut().push(path.clone());

            let call = self.calls(path);
            let (_, outcomes) = self.paths.iter()
                .find(|(p, _)| p == path)
                .expect("a path the test names");
            let outcome = outcomes.get(call - 1).or(outcomes.last()).copied().flatten();

            outcome.ok_or_else(|| Error::SensorNoReading(path.clone()))
        }

        fn calls(&self, path: &str) -> usize {
            self.calls.borrow().iter().filter(|p| *p == path).count()
        }
    }

    fn celsius(temp: u32) -> Option<SensorReading> {
        Some(SensorReading { value: SensorValue::Uint32(temp), units: SensorUnits::Celsius })
    }

    fn sensor_map(sensors: &[(&str, Option<SensorReading>)]) -> HashMap<String, Option<SensorReading>> {
        sensors.iter().map(|(name, reading)| (name.to_string(), *reading)).collect()
    }

    fn query_error() -> Error {
        Error::Io { path: "(ipmi)".into(), source: io::Error::other("BMC busy") }
    }

    /// Read `zone`, none of whose sources failed in the last poll, with a
    /// query that always returns `sensors`, and count the queries
    fn read_with_sensors(zone: &Zone, sensors: &[(&str, Option<SensorReading>)], reader: &FakeReader)
        -> (Result<Vec<SourceReading>, Error>, usize)
    {
        read_after(zone, &vec![false; zone.sources.len()], sensors, reader)
    }

    /// Read `zone` as [`read_with_sensors`] does, after a poll in which the
    /// sources marked in `failed_last` failed
    fn read_after(
        zone: &Zone,
        failed_last: &[bool],
        sensors: &[(&str, Option<SensorReading>)],
        reader: &FakeReader,
    ) -> (Result<Vec<SourceReading>, Error>, usize) {
        let mut queries = 0;
        let mut query = || {
            queries += 1;
            Ok(sensor_map(sensors))
        };
        let result = read_zone_sources(zone, failed_last, &mut query, &|s| reader.read(s));

        (result, queries)
    }

    /// A query that returns `maps` in turn and repeats the last one, counting
    /// its calls in `queries`
    fn changing_query<'a>(maps: &'a [HashMap<String, Option<SensorReading>>], queries: &'a mut usize)
        -> impl FnMut() -> Result<HashMap<String, Option<SensorReading>>, Error> + 'a
    {
        move || {
            *queries += 1;
            Ok(maps.get(*queries - 1).or(maps.last()).cloned().unwrap_or_default())
        }
    }

    /// Read `zone` as the zone loop does, after the polls `state` has seen
    fn poll(zone: &Zone, state: &mut ZoneState, reader: &FakeReader) -> Vec<SourceReading> {
        let failed_last = state.failed_sources();
        let readings = read_zone_sources(zone, &failed_last, &mut || Ok(HashMap::new()), &|s| reader.read(s))
            .unwrap();
        state.next_duty(zone, &readings);

        readings
    }

    fn failed_text(attempts: u64, path: &str) -> SourceReading {
        Failed(format!("Failed all {} attempt(s); last attempt error: Sensor reading not available: {}", attempts, path))
    }

    #[test]
    fn source_failed_last_poll_is_read_once_and_leaves_the_waits() {
        let zone = zone(vec![file("gpu", IGNORE), file("disk", OnFailure::Exit {})], 2);
        let reader = FakeReader::new(&[("gpu", 9, 70), ("disk", 2, 35)]);

        let (result, _) = read_after(&zone, &[true, false], &[], &reader);

        assert_eq!(result.unwrap(), [failed_text(1, "gpu"), Temp(35)]);
        assert_eq!(reader.calls("gpu"), 1);
        assert_eq!(reader.calls("disk"), 3);
    }

    #[test]
    fn failed_source_gets_its_retries_again_once_it_reads() {
        let zone = zone(vec![file("gpu", IGNORE)], 2);
        // Fails a whole poll and once more, reads, then fails twice and reads
        let reader = FakeReader::scripted(&[("gpu", &[None, None, None, None, Some(70), None, None, Some(72)])]);
        let mut state = ZoneState::new(&zone);

        assert_eq!(poll(&zone, &mut state, &reader), [failed_text(3, "gpu")]);
        assert_eq!(reader.calls("gpu"), 3);
        assert_eq!(poll(&zone, &mut state, &reader), [failed_text(1, "gpu")]);
        assert_eq!(reader.calls("gpu"), 4);
        assert_eq!(poll(&zone, &mut state, &reader), [Temp(70)]);
        assert_eq!(reader.calls("gpu"), 5);
        assert_eq!(poll(&zone, &mut state, &reader), [Temp(72)]);
        assert_eq!(reader.calls("gpu"), 8);
    }

    #[test]
    fn missing_sensor_failed_last_poll_makes_one_query() {
        let zone = zone(vec![ipmi("Missing", IGNORE), ipmi(CPU, OnFailure::Exit {})], 2);

        let (result, queries) = read_after(&zone, &[true, false], &[(CPU, celsius(50))], &FakeReader::new(&[]));

        assert!(matches!(&result.as_ref().unwrap()[..],
            [Failed(e), Temp(50)] if e == "Failed all 1 attempt(s); last attempt error: Sensor not found: Missing"),
            "{:?}", result);
        assert_eq!(queries, 1);
    }

    #[test]
    fn failed_query_for_sensors_failed_last_poll_is_not_retried() {
        let zone = zone(vec![ipmi("GPU Temp", IGNORE)], 2);
        let mut queries = 0;
        let mut query = || {
            queries += 1;
            Err(query_error())
        };

        let readings = read_zone_sources(&zone, &[true], &mut query, &|_| panic!("no file source")).unwrap();

        assert!(matches!(&readings[..], [Failed(_)]), "{:?}", readings);
        assert_eq!(queries, 1);
    }

    #[test]
    fn sensor_failed_last_poll_takes_value_of_retry_for_another_sensor() {
        let zone = zone(vec![ipmi("GPU Temp", IGNORE), ipmi(CPU, IGNORE)], 2);
        let maps = [sensor_map(&[]), sensor_map(&[("GPU Temp", celsius(60)), (CPU, celsius(50))])];
        let mut queries = 0;

        let readings = read_zone_sources(&zone, &[true, false], &mut changing_query(&maps, &mut queries),
            &|_| panic!("no file source")).unwrap();

        assert_eq!(readings, [Temp(60), Temp(50)]);
        assert_eq!(queries, 2);
    }

    #[test]
    fn other_sources_are_read_before_the_ipmi_query() {
        let zone = zone(vec![ipmi(CPU, OnFailure::Exit {}), file("disk", OnFailure::Exit {})], 0);
        let events = RefCell::new(Vec::new());
        let mut query = || {
            events.borrow_mut().push("query");
            Ok(sensor_map(&[(CPU, celsius(50))]))
        };
        let reader = |_: &Source| {
            events.borrow_mut().push("disk");
            Ok(35)
        };

        let readings = read_zone_sources(&zone, &[false, false], &mut query, &reader).unwrap();

        assert_eq!(readings, [Temp(50), Temp(35)]);
        assert_eq!(events.into_inner(), ["disk", "query"]);
    }

    #[test]
    fn exit_source_keeps_its_retries_beside_sensor_failed_last_poll() {
        let zone = zone(vec![file("gpu", OnFailure::Exit {}), ipmi("Missing", IGNORE)], 2);
        let reader = FakeReader::new(&[("gpu", 1, 70)]);

        let (result, queries) = read_after(&zone, &[false, true], &[], &reader);

        assert!(matches!(&result.as_ref().unwrap()[..], [Temp(70), Failed(_)]), "{:?}", result);
        assert_eq!(queries, 1);
    }

    #[test]
    fn exhausted_exit_source_makes_no_ipmi_query() {
        let zone = zone(vec![ipmi(CPU, IGNORE), file("gpu", OnFailure::Exit {})], 1);
        let reader = FakeReader::new(&[("gpu", 9, 70)]);

        let (result, queries) = read_with_sensors(&zone, &[(CPU, celsius(50))], &reader);

        assert_retries_failed(&result, 2, "Sensor reading not available: gpu");
        assert_eq!(queries, 0);
    }

    #[test]
    fn ipmi_batch_and_other_sources_share_one_retry_budget() {
        // The disk takes both waits, so the sensor missing from the first map
        // gets no second query
        let zone = zone(vec![ipmi(CPU, IGNORE), file("disk", IGNORE)], 2);
        let reader = FakeReader::new(&[("disk", 2, 35)]);
        let maps = [sensor_map(&[]), sensor_map(&[(CPU, celsius(50))])];
        let mut queries = 0;

        let readings = read_zone_sources(&zone, &[false, false], &mut changing_query(&maps, &mut queries),
            &|s| reader.read(s)).unwrap();

        assert!(matches!(&readings[..], [Failed(_), Temp(35)]), "{:?}", readings);
        assert_eq!(queries, 1);
        assert_eq!(reader.calls("disk"), 3);
    }

    #[test]
    fn ipmi_retry_keeps_first_values_and_takes_later_value_of_missing_sensor() {
        let zone = zone(vec![ipmi(CPU, IGNORE), ipmi("GPU Temp", IGNORE)], 2);
        let maps = [
            sensor_map(&[(CPU, celsius(50))]),
            sensor_map(&[(CPU, celsius(60)), ("GPU Temp", celsius(55))]),
        ];
        let mut queries = 0;

        let readings = read_zone_sources(&zone, &[false, false], &mut changing_query(&maps, &mut queries),
            &|_| panic!("no file source")).unwrap();

        assert_eq!(readings, [Temp(50), Temp(55)]);
        assert_eq!(queries, 2);
    }

    fn assert_retries_failed(result: &Result<Vec<SourceReading>, Error>, attempts: u64, message: &str) {
        assert!(matches!(result,
            Err(Error::RetriesFailed { attempts: a, source }) if *a == attempts && source.to_string() == message),
            "{:?}", result);
    }

    #[test]
    fn source_failing_twice_gives_its_reading_and_others_read_once() {
        let zone = zone(vec![ipmi(CPU, OnFailure::Exit {}), file("gpu", OnFailure::Exit {}), file("disk", OnFailure::Exit {})], 2);
        let reader = FakeReader::new(&[("gpu", 2, 70), ("disk", 0, 35)]);

        let (result, queries) = read_with_sensors(&zone, &[(CPU, celsius(50))], &reader);

        assert_eq!(result.unwrap(), [Temp(50), Temp(70), Temp(35)]);
        assert_eq!(queries, 1);
        assert_eq!(reader.calls("gpu"), 3);
        assert_eq!(reader.calls("disk"), 1);
    }

    #[test]
    fn exhausted_exit_source_returns_its_error_and_reads_no_later_source() {
        let zone = zone(vec![file("gpu", OnFailure::default()), file("disk", IGNORE)], 2);
        let reader = FakeReader::new(&[("gpu", 9, 70), ("disk", 0, 35)]);

        let (result, _) = read_with_sensors(&zone, &[], &reader);

        assert_retries_failed(&result, 3, "Sensor reading not available: gpu");
        assert_eq!(reader.calls("gpu"), 3);
        assert_eq!(reader.calls("disk"), 0);
    }

    #[test]
    fn exhausted_exit_ipmi_source_returns_its_error_after_other_sources() {
        // The IPMI batch comes last, so the disk has been read once by then
        let zone = zone(vec![file("disk", IGNORE), ipmi("GPU Temp", OnFailure::Exit {}), ipmi(CPU, IGNORE)], 2);
        let reader = FakeReader::new(&[("disk", 0, 35)]);

        let (result, queries) = read_with_sensors(&zone, &[(CPU, celsius(50))], &reader);

        assert_retries_failed(&result, 3, "Sensor not found: GPU Temp");
        assert_eq!(queries, 3);
        assert_eq!(*reader.calls.borrow(), ["disk"]);
    }

    #[test]
    fn exhausted_ignore_source_gives_failed_reading_with_error_text() {
        let zone = zone(vec![file("gpu", IGNORE), file("disk", OnFailure::Dcycle { dcycle: 80 })], 2);
        let reader = FakeReader::new(&[("gpu", 9, 70), ("disk", 9, 35)]);

        let (result, _) = read_with_sensors(&zone, &[], &reader);

        assert_eq!(result.unwrap(), [
            Failed("Failed all 3 attempt(s); last attempt error: Sensor reading not available: gpu".to_owned()),
            Failed("Failed all 1 attempt(s); last attempt error: Sensor reading not available: disk".to_owned()),
        ]);
    }

    #[test]
    fn retries_are_shared_by_the_zone_sources() {
        // The first source takes both waits, so the second gets one attempt
        let zone = zone(vec![file("gpu", IGNORE), file("disk", IGNORE)], 2);
        let reader = FakeReader::new(&[("gpu", 9, 70), ("disk", 1, 35)]);

        let (result, _) = read_with_sensors(&zone, &[], &reader);

        assert!(matches!(result.unwrap()[..], [Failed(_), Failed(_)]));
        assert_eq!(reader.calls("gpu"), 3);
        assert_eq!(reader.calls("disk"), 1);
    }

    #[test]
    fn missing_ipmi_sensor_with_ignore_leaves_other_sensor_in_use() {
        let zone = zone(vec![ipmi("GPU Temp", IGNORE), ipmi(CPU, OnFailure::Exit {})], 2);
        let reader = FakeReader::new(&[]);

        let (result, queries) = read_with_sensors(&zone, &[(CPU, celsius(50))], &reader);
        let readings = result.unwrap();

        assert!(matches!(&readings[..], [Failed(e), Temp(50)] if e.contains("Sensor not found: GPU Temp")),
            "{:?}", readings);
        // One query, then one more for each retry of the missing sensor
        assert_eq!(queries, 3);
        // CPU 50 C gives 54 on the zone curve
        assert_eq!(ZoneState::new(&zone).next_duty(&zone, &readings).duty, 54);
    }

    #[test]
    fn failed_query_with_ignore_and_exit_sensors_returns_error() {
        let zone = zone(vec![ipmi("GPU Temp", IGNORE), ipmi(CPU, OnFailure::Exit {})], 2);
        let mut queries = 0;
        let mut query = || {
            queries += 1;
            Err(query_error())
        };

        let result = read_zone_sources(&zone, &[false, false], &mut query, &|_| panic!("no file source"));

        assert_retries_failed(&result, 3, "\"(ipmi)\": BMC busy");
        assert_eq!(queries, 3);
    }

    #[test]
    fn failed_query_gives_failed_reading_to_each_ignore_sensor() {
        let zone = zone(vec![ipmi("GPU Temp", IGNORE), ipmi(CPU, OnFailure::Dcycle { dcycle: 80 }), file("disk", IGNORE)], 1);
        let reader = FakeReader::new(&[("disk", 0, 35)]);
        let mut query = || Err(query_error());

        let readings = read_zone_sources(&zone, &[false, false, false], &mut query, &|s| reader.read(s)).unwrap();
        let failed = Failed("Failed all 2 attempt(s); last attempt error: \"(ipmi)\": BMC busy".to_owned());

        assert_eq!(readings, [failed.clone(), failed, Temp(35)]);
    }

    #[test]
    fn query_failing_once_is_retried() {
        let zone = zone(vec![ipmi(CPU, OnFailure::Exit {})], 2);
        let mut queries = 0;
        let mut query = || {
            queries += 1;
            if queries == 1 {
                return Err(query_error());
            }
            Ok(sensor_map(&[(CPU, celsius(50))]))
        };

        let readings = read_zone_sources(&zone, &[false], &mut query, &|_| panic!("no file source")).unwrap();

        assert_eq!(readings, [Temp(50)]);
        assert_eq!(queries, 2);
    }

    #[test]
    fn two_sources_on_one_missing_sensor_each_fail() {
        let zone = zone(vec![ipmi("GPU Temp", IGNORE), ipmi("GPU Temp", OnFailure::Dcycle { dcycle: 80 })], 0);

        let (result, queries) = read_with_sensors(&zone, &[(CPU, celsius(50))], &FakeReader::new(&[]));

        assert!(matches!(result.unwrap()[..], [Failed(_), Failed(_)]));
        assert_eq!(queries, 1);
    }

    #[test]
    fn two_sources_on_one_sensor_each_read_it() {
        let zone = zone(vec![ipmi(CPU, IGNORE), ipmi(CPU, OnFailure::Exit {})], 0);

        let (result, _) = read_with_sensors(&zone, &[(CPU, celsius(50))], &FakeReader::new(&[]));

        assert_eq!(result.unwrap(), [Temp(50), Temp(50)]);
    }

    #[test]
    fn zone_without_ipmi_sources_makes_no_query() {
        let zone = zone(vec![file("disk", OnFailure::Exit {})], 2);

        let (result, queries) = read_with_sensors(&zone, &[], &FakeReader::new(&[("disk", 0, 35)]));

        assert_eq!(result.unwrap(), [Temp(35)]);
        assert_eq!(queries, 0);
    }

    #[test]
    fn ipmi_sensor_checks() {
        let readings = sensor_map(&[
            (CPU, celsius(50)),
            ("Fractional", Some(SensorReading { value: SensorValue::Double(45.7), units: SensorUnits::Celsius })),
            ("Empty", None),
            ("Fahrenheit", Some(SensorReading { value: SensorValue::Uint32(120), units: SensorUnits::Fahrenheit })),
            ("Switch", Some(SensorReading { value: SensorValue::Bool(true), units: SensorUnits::Celsius })),
            ("Hot", celsius(256)),
        ]);

        assert_eq!(ipmi_sensor_temp(CPU, &readings).unwrap(), 50);
        assert_eq!(ipmi_sensor_temp("Fractional", &readings).unwrap(), 45);
        assert!(matches!(ipmi_sensor_temp("Missing", &readings), Err(Error::SensorNotFound(s)) if s == "Missing"));
        assert!(matches!(ipmi_sensor_temp("Empty", &readings), Err(Error::SensorNoReading(s)) if s == "Empty"));
        assert!(matches!(ipmi_sensor_temp("Fahrenheit", &readings),
            Err(Error::SensorBadUnits { units: SensorUnits::Fahrenheit, .. })));
        assert!(matches!(ipmi_sensor_temp("Switch", &readings), Err(Error::SensorBadValue { .. })));
        assert!(matches!(ipmi_sensor_temp("Hot", &readings),
            Err(Error::SensorValueOutOfRange { sensor, value: SensorValue::Uint32(256) }) if sensor == "Hot"));
    }

    fn double(value: f64) -> Option<SensorReading> {
        Some(SensorReading { value: SensorValue::Double(value), units: SensorUnits::Celsius })
    }

    #[test]
    fn ipmi_value_outside_u8_is_out_of_range() {
        let cases = [
            ("NaN", f64::NAN),
            ("Infinite", f64::INFINITY),
            ("Negative infinite", f64::NEG_INFINITY),
            ("Negative", -1.0),
            ("Fraction below zero", -0.5),
            ("Too hot", 256.0),
            ("Far too hot", 1e20),
        ];
        let readings = sensor_map(&cases.map(|(name, value)| (name, double(value))));

        let wrong: Vec<_> = cases.iter()
            .map(|(name, _)| (name, ipmi_sensor_temp(name, &readings)))
            .filter(|(name, result)| !matches!(result,
                Err(Error::SensorValueOutOfRange { sensor, value: SensorValue::Double(_) }) if sensor == *name))
            .collect();

        assert!(wrong.is_empty(), "{:?}", wrong);
    }

    #[test]
    fn ipmi_fraction_below_256_truncates() {
        let readings = sensor_map(&[("Zero", double(0.0)), ("Highest", double(255.9))]);

        assert_eq!(ipmi_sensor_temp("Zero", &readings).unwrap(), 0);
        assert_eq!(ipmi_sensor_temp("Highest", &readings).unwrap(), 255);
    }
}
