use {
    std::{
        collections::{HashMap, HashSet},
        convert::TryInto,
        fs,
        io::{BufRead, BufReader, Read},
        path::{Path, PathBuf},
        process::{Child, Command, ExitStatus, Stdio},
        sync::{Arc, Mutex},
        thread,
        time::{Duration, Instant},
    },
    crate::{
        config::Source,
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
/// --format=csv,noheader,nounits` for one GPU. The output must be a single
/// line holding an integer in degrees Celsius.
fn parse_nvidia_output(gpu: &str, stdout: &str) -> Result<u8> {
    let bad_data = || Error::NvidiaBadData { gpu: gpu.to_owned(), output: stdout.to_owned() };
    let trimmed = stdout.trim();

    if trimmed.lines().count() != 1 {
        return Err(bad_data());
    }

    let temperature = trimmed
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

/// Kill a child that may still be running and wait for it, so that it does
/// not linger as a zombie. Kill fails if the child has already exited, which
/// is harmless, so only the error from the wait is returned.
fn kill_and_reap(child: &mut Child) -> std::io::Result<ExitStatus> {
    let _ = child.kill();
    child.wait()
}

/// Run a command with piped stdout and return its exit status and output. If
/// the command has not exited within `limit`, it is killed and reaped, and
/// [`Error::CommandTimeout`] is returned.
///
/// stdout is read only after the child exits. This suits commands whose
/// output fits in the pipe buffer, such as nvidia-smi's few bytes; a command
/// that writes more would block on the full pipe and time out. The read also
/// waits for any descendant that inherited the pipe, which nvidia-smi leaves
/// none of.
fn run_with_timeout(cmd: &mut Command, limit: Duration) -> Result<(ExitStatus, String)> {
    let program = PathBuf::from(cmd.get_program());
    let io_error = |e| Error::Io { path: program.clone(), source: e };

    let mut child = cmd
        .stdout(Stdio::piped())
        .spawn()
        .map_err(io_error)?;
    let deadline = Instant::now() + limit;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {},
            Err(e) => {
                let _ = kill_and_reap(&mut child);
                return Err(io_error(e));
            }
        }

        if Instant::now() >= deadline {
            kill_and_reap(&mut child).map_err(io_error)?;
            return Err(Error::CommandTimeout {
                command: program.display().to_string(),
                seconds: limit.as_secs(),
            });
        }

        thread::sleep(CHILD_POLL_INTERVAL);
    };

    let mut stdout = String::new();

    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_string(&mut stdout).map_err(io_error)?;
    }

    Ok((status, stdout))
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
pub fn get_source_readings(ipmi: Arc<Mutex<Ipmi>>, sources: &[Source])
    -> Result<Vec<u8>>
{
    // Get IPMI sensor readings in one go for better performance.
    let ipmi_sensors = sources.iter()
        .filter_map(|s| {
            match s {
                Source::Ipmi { sensor } => Some(sensor.clone()),
                _ => None,
            }
        })
        .collect::<HashSet<_>>();

    let ipmi_results = parse_ipmi_sources(ipmi, &ipmi_sensors)?;

    sources.iter()
        .map(|s| {
            match s {
                Source::Ipmi { sensor } => Ok(ipmi_results[sensor.as_str()]),
                Source::File { path } => parse_file_source(path),
                Source::Smart { block_dev } => parse_smart_source(block_dev),
                Source::Hdparm { block_dev } => parse_hdparm_source(block_dev),
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
            os::unix::{fs::PermissionsExt, process::ExitStatusExt},
            path::{Path, PathBuf},
            process::{self, ExitStatus},
            sync::{
                atomic::{AtomicUsize, Ordering},
                Mutex,
            },
            time::{Duration, Instant},
        },
        serde_json::json,
        super::{
            parse_file_source,
            parse_hdparm_output,
            parse_nvidia_output,
            parse_smart_output,
            read_nvidia_with_limit,
            smart_reading,
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
    fn nvidia_temperature_above_u8_exceeds_bounds() {
        let result = parse_nvidia_output(GPU, "300\n");

        assert!(matches!(result, Err(Error::ReadingExceedsBounds)), "{:?}", result);
    }

    const NVIDIA_TEST_LIMIT: Duration = Duration::from_millis(300);

    // Serialises the tests that write and run a script. A fork in another
    // test thread can inherit the script's write descriptor for a moment, and
    // exec of the script then fails with ETXTBSY.
    static SCRIPT_LOCK: Mutex<()> = Mutex::new(());

    /// Run `body` as a fake nvidia-smi script in `dir`
    fn nvidia_script_reading(dir: &TempDir, body: &str) -> (PathBuf, Result<u8, Error>) {
        let _guard = SCRIPT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let script = dir.write("nvidia-smi", &format!("#!/bin/sh\n{}\n", body));
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        let result = read_nvidia_with_limit(&script, GPU, NVIDIA_TEST_LIMIT);
        (script, result)
    }

    #[test]
    fn nvidia_script_output_gives_reading() {
        let dir = TempDir::new();
        let body = format!(
            "[ \"$*\" = \"-i {} --query-gpu=temperature.gpu --format=csv,noheader,nounits\" ] || exit 3\necho 71",
            GPU);
        let (_, result) = nvidia_script_reading(&dir, &body);

        assert_eq!(result.unwrap(), 71);
    }

    #[test]
    fn nvidia_script_failure_is_command_error() {
        let dir = TempDir::new();
        let (script, result) = nvidia_script_reading(&dir, "exit 9");

        assert!(matches!(&result,
            Err(Error::Command { command, status })
                if *command == script && status.code() == Some(9)),
            "{:?}", result);
    }

    #[test]
    fn nvidia_script_timeout_kills_child() {
        let dir = TempDir::new();
        let pid_file = dir.0.join("pid");
        // exec keeps the shell's pid, so the recorded pid is the sleeping process
        let body = format!("echo $$ > '{}'\nexec sleep 30", pid_file.display());

        let start = Instant::now();
        let (script, result) = nvidia_script_reading(&dir, &body);
        let elapsed = start.elapsed();

        assert!(matches!(&result,
            Err(Error::CommandTimeout { command, seconds: 0 })
                if Path::new(command) == script),
            "{:?}", result);
        assert!(elapsed < NVIDIA_TEST_LIMIT * 10, "returned after {:?}", elapsed);

        // A zombie still has a /proc entry, so this also proves the child was reaped
        let pid = fs::read_to_string(&pid_file).unwrap();
        assert!(!Path::new("/proc").join(pid.trim()).exists(), "pid {} still exists", pid.trim());
    }
}
