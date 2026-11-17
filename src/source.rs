use {
    std::{
        collections::{HashMap, HashSet},
        convert::TryInto,
        fs,
        io::{BufRead, BufReader, Read},
        path::Path,
        process::{Command, ExitStatus, Stdio},
        sync::{Arc, Mutex},
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
            os::unix::process::ExitStatusExt,
            path::{Path, PathBuf},
            process::{self, ExitStatus},
            sync::atomic::{AtomicUsize, Ordering},
        },
        serde_json::json,
        super::{parse_file_source, parse_hdparm_output, parse_smart_output, smart_reading},
        crate::error::Error,
    };

    const BLOCK_DEV: &str = "/dev/sda";
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
}
