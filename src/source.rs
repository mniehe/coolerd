use {
    std::{
        collections::HashMap,
        convert::TryInto,
        ffi::OsStr,
        fs,
        io::BufRead,
        iter,
        path::Path,
        process::ExitStatus,
        str,
        thread,
        time::Duration,
    },
    log::trace,
    crate::{
        command::{CommandRunner, RunLimits},
        config::{OnFailure, Source, SourceConfig, Zone},
        curve::SourceReading,
        error::{Error, Result},
        freeipmi::{SensorReading, SensorUnits, SensorValue},
        hwmon::{HwmonRoot, read_number, read_optional_number},
    },
};

/// Longest wait for one smartctl or hdparm run. Waking a spun-down disk can
/// take many seconds.
const DISK_TOOL_TIMEOUT: Duration = Duration::from_secs(30);

/// Most bytes of smartctl or hdparm output read. smartctl's JSON attribute
/// table is often longer than [`NVIDIA_OUTPUT_CAP`].
const DISK_TOOL_OUTPUT_CAP: u64 = 1024 * 1024;

/// Longest wait for one nvidia-smi run
const NVIDIA_SMI_TIMEOUT: Duration = Duration::from_secs(5);

/// Most bytes of nvidia-smi output read
const NVIDIA_OUTPUT_CAP: u64 = 4096;

/// Get the temperature of a hard drive via smartctl. This function fails if
/// smartctl does not return temperature data (eg. if a drive is in standby),
/// does not exit within [`DISK_TOOL_TIMEOUT`], or if the reported temperature
/// does not fit in a [`u8`].
fn parse_smart_source(runner: &mut CommandRunner, block_dev: &Path) -> Result<u8> {
    read_smart_with_limit(runner, Path::new("smartctl"), block_dev, DISK_TOOL_TIMEOUT)
}

/// Get the temperature of a hard drive via `program`, a smartctl, waiting at
/// most `limit` for it to exit
fn read_smart_with_limit(runner: &mut CommandRunner, program: &Path, block_dev: &Path, limit: Duration)
    -> Result<u8>
{
    let args = [
        OsStr::new("-j"),
        OsStr::new("-A"),
        OsStr::new("-n"),
        OsStr::new("standby"),
        block_dev.as_os_str(),
    ];

    let (status, stdout) = runner.run(program, &args, RunLimits { time: limit, output: DISK_TOOL_OUTPUT_CAP })?;

    smart_reading(status, block_dev, &stdout)
}

/// Turn smartctl's exit status and its JSON output into a reading. A failing
/// exit status takes precedence over output that does not parse, including
/// output that is not UTF-8.
fn smart_reading(status: ExitStatus, block_dev: &Path, stdout: &[u8]) -> Result<u8> {
    match status.code() {
        // smartctl will return status code 2 when a drive is in standby
        Some(0) | Some(2) => {},
        _ => return Err(Error::Command { command: "smartctl".into(), status }),
    }

    let root = serde_json::from_slice(stdout)
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

/// Find the temperature in hdparm's output, read line by line. Lines after
/// the temperature line are not read, so they need not be UTF-8.
fn parse_hdparm_output(block_dev: &Path, stdout: &[u8]) -> Result<u8> {
    let mut reader = stdout;
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
/// sense data line, hdparm does not exit within [`DISK_TOOL_TIMEOUT`], or if
/// the reported temperature does not fit in a [`u8`].
fn parse_hdparm_source(runner: &mut CommandRunner, block_dev: &Path) -> Result<u8> {
    read_hdparm_with_limit(runner, Path::new("hdparm"), block_dev, DISK_TOOL_TIMEOUT)
}

/// Get the temperature of a drive via `program`, an hdparm, waiting at most
/// `limit` for it to exit
fn read_hdparm_with_limit(runner: &mut CommandRunner, program: &Path, block_dev: &Path, limit: Duration)
    -> Result<u8>
{
    let args = [OsStr::new("-H"), block_dev.as_os_str()];

    let (status, stdout) = runner.run(program, &args, RunLimits { time: limit, output: DISK_TOOL_OUTPUT_CAP })?;
    let result = parse_hdparm_output(block_dev, &stdout);

    if status.code() != Some(0) {
        return Err(Error::Command { command: program.to_owned(), status });
    }

    result
}

/// Parse the output of `nvidia-smi --query-gpu=temperature.gpu
/// --format=csv,noheader,nounits` for one GPU. The output must be one record
/// of ASCII digits, the temperature in degrees Celsius, followed by at most
/// one line ending.
fn parse_nvidia_output(gpu: &str, stdout: &[u8]) -> Result<u8> {
    let bad_data = || Error::NvidiaBadData {
        gpu: gpu.to_owned(),
        output: String::from_utf8_lossy(stdout).into_owned(),
    };
    let record = stdout.strip_suffix(b"\r\n")
        .or_else(|| stdout.strip_suffix(b"\n"))
        .unwrap_or(stdout);

    if record.is_empty() || !record.iter().all(u8::is_ascii_digit) {
        return Err(bad_data());
    }

    let temperature = str::from_utf8(record)
        .map_err(|_| bad_data())?
        .parse::<u32>()
        .map_err(|_| bad_data())?
        .try_into()
        .map_err(|_| Error::ReadingExceedsBounds)?;

    Ok(temperature)
}

/// Get the temperature of one NVIDIA GPU via nvidia-smi, waiting at most
/// `limit` for nvidia-smi to exit. `gpu` is a PCI bus id or a GPU UUID.
fn read_nvidia_with_limit(runner: &mut CommandRunner, program: &Path, gpu: &str, limit: Duration)
    -> Result<u8>
{
    let args = [
        OsStr::new("-i"),
        OsStr::new(gpu),
        OsStr::new("--query-gpu=temperature.gpu"),
        OsStr::new("--format=csv,noheader,nounits"),
    ];

    let (status, stdout) = runner.run(program, &args, RunLimits { time: limit, output: NVIDIA_OUTPUT_CAP })?;

    if status.code() != Some(0) {
        return Err(Error::Command { command: program.to_owned(), status });
    }

    parse_nvidia_output(gpu, &stdout)
}

/// Get the temperature of one NVIDIA GPU via nvidia-smi. This function fails
/// if nvidia-smi exits with a non-zero status, does not exit within
/// [`NVIDIA_SMI_TIMEOUT`], or does not print a temperature that fits in a
/// [`u8`].
fn read_nvidia_source(runner: &mut CommandRunner, gpu: &str) -> Result<u8> {
    read_nvidia_with_limit(runner, Path::new("nvidia-smi"), gpu, NVIDIA_SMI_TIMEOUT)
}

/// Get the temperature from a plain-text file (typically a sysfs path). The
/// contents of the file should be a decimal-formatted integer in units of
/// thousandths degrees Celsius after whitespace is trimmed. If the temperature,
/// after being converted to degrees Celsius, does not fit in a [`u8`], then
/// [`Error::ReadingExceedsBounds`] is returned. A path that is not a regular
/// file, after symlinks are followed, gives [`Error::NotRegularFile`], because
/// reading a FIFO without a writer would block the zone for ever.
fn parse_file_source<T: AsRef<Path>>(path: T) -> Result<u8> {
    let path = path.as_ref();
    let io_error = |e| Error::Io { path: path.to_owned(), source: e };

    if !fs::metadata(path).map_err(io_error)?.file_type().is_file() {
        return Err(Error::NotRegularFile(path.to_owned()));
    }

    let contents = fs::read_to_string(path).map_err(io_error)?;
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

/// Convert a hwmon temperature in millidegrees Celsius to whole degrees,
/// rounding up and clamping above to the range of a reading. Returns `None`
/// for a negative value, because an open probe reports a negative sentinel
/// that would otherwise pass as a cool reading.
fn millidegrees_to_celsius(raw: i64) -> Option<u8> {
    const MILLIDEGREES_PER_DEGREE: i64 = 1000;

    if raw < 0 {
        return None;
    }

    let degrees = raw
        .saturating_add(MILLIDEGREES_PER_DEGREE - 1)
        .div_euclid(MILLIDEGREES_PER_DEGREE)
        .min(u8::MAX.into());

    // The bounds leave a value that fits
    Some(u8::try_from(degrees).unwrap_or(u8::MAX))
}

/// Read input `input` of the hwmon chip named `chip`. The chip is found again
/// on every read, so a chip that moves to another `hwmonN` keeps reading. A
/// non-zero `temp<input>_fault` fails the read; a chip without one has none.
fn read_hwmon_source(root: &HwmonRoot, chip: &str, input: u8) -> Result<u8> {
    let device = root.resolve(chip)?;
    let fault_path = device.join(format!("temp{}_fault", input));

    if read_optional_number(&fault_path)?.is_some_and(|fault| fault != 0) {
        return Err(Error::HwmonFault { path: fault_path });
    }

    let path = device.join(format!("temp{}_input", input));
    let raw = read_number(&path)?;

    millidegrees_to_celsius(raw).ok_or(Error::HwmonValueOutOfRange { path, value: raw })
}

/// The readers of one zone's sources. Each source that runs a program has
/// its own [`CommandRunner`], so a program stuck after its kill blocks only
/// its own source. Each zone loop owns one, so zones share no runner.
#[derive(Default)]
pub struct ZoneReaders {
    /// By source index, as in [`Zone::sources`]; made on a source's first read
    runners: Vec<CommandRunner>,
    /// Where `hwmon` sources find their chips
    hwmon: HwmonRoot,
}

impl ZoneReaders {
    pub fn new(hwmon: HwmonRoot) -> Self {
        Self { runners: Vec::new(), hwmon }
    }

    /// Read `source`, the source at `index` of the zone, which is not an IPMI
    /// sensor
    pub fn read(&mut self, index: usize, source: &Source) -> Result<u8> {
        match source {
            // read_zone_sources reads IPMI sensors in one batch and never
            // passes one here
            Source::Ipmi { sensor } => Err(Error::SensorNotFound(sensor.clone())),
            Source::File { path } => parse_file_source(path),
            Source::Smart { block_dev } => parse_smart_source(self.runner(index), Path::new(block_dev)),
            Source::Hdparm { block_dev } => parse_hdparm_source(self.runner(index), Path::new(block_dev)),
            Source::Nvidia { gpu } => read_nvidia_source(self.runner(index), gpu),
            Source::Hwmon { chip, input } => read_hwmon_source(&self.hwmon, chip, *input),
        }
    }

    fn runner(&mut self, index: usize) -> &mut CommandRunner {
        if self.runners.len() <= index {
            self.runners.resize_with(index + 1, CommandRunner::default);
        }

        &mut self.runners[index]
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

/// Read the source at `index` with `reader`, taking a wait from `waits`
/// before each retry
fn read_with_retries(
    index: usize,
    source: &SourceConfig,
    reader: &mut dyn FnMut(usize, &Source) -> Result<u8>,
    waits: &mut Waits,
) -> Result<SourceReading> {
    let mut attempts = 0;

    loop {
        attempts += 1;

        let error = match reader(index, &source.kind) {
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
/// `reader` reads each source that is not an IPMI sensor, in source order,
/// and is given the source's index in `zone.sources` with it;
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
    reader: &mut dyn FnMut(usize, &Source) -> Result<u8>,
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

        readings[index] = Some(read_with_retries(index, source, reader, source_waits)?);
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
            fs,
            io,
            os::unix::{fs::symlink, process::ExitStatusExt},
            path::{Path, PathBuf},
            process::{Command, ExitStatus},
            sync::mpsc,
            thread,
            time::{Duration, Instant},
        },
        serde_json::json,
        super::{
            NVIDIA_OUTPUT_CAP,
            ZoneReaders,
            ipmi_sensor_temp,
            millidegrees_to_celsius,
            parse_file_source,
            parse_hdparm_output,
            parse_nvidia_output,
            parse_smart_output,
            read_hdparm_with_limit,
            read_nvidia_with_limit,
            read_smart_with_limit,
            read_zone_sources,
            smart_reading,
        },
        crate::{
            command::{
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
            hwmon::HwmonRoot,
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
        smart_reading(exit_status(code), Path::new(BLOCK_DEV), output.as_bytes())
    }

    fn file_reading(contents: &str) -> Result<u8, Error> {
        let dir = TempDir::new();
        let path = dir.write("temp", contents);
        parse_file_source(path)
    }

    fn hdparm_reading(output: &str) -> Result<u8, Error> {
        parse_hdparm_output(Path::new(BLOCK_DEV), output.as_bytes())
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

    #[test]
    fn file_through_symlink_reads() {
        let dir = TempDir::new();
        let target = dir.write("temp", "42000\n");
        let link = dir.0.join("link");
        symlink(&target, &link).unwrap();

        assert_eq!(parse_file_source(&link).unwrap(), 42);
    }

    /// Read `path` on another thread, failing the test if the read blocks
    fn bounded_file_reading(path: &Path) -> Result<u8, Error> {
        let (sender, receiver) = mpsc::channel();
        let reader_path = path.to_owned();
        thread::spawn(move || sender.send(parse_file_source(reader_path)));

        receiver.recv_timeout(FINISH_LIMIT).expect("the read blocked")
    }

    #[test]
    fn file_that_is_a_fifo_is_error_without_blocking() {
        let dir = TempDir::new();
        let fifo = dir.0.join("fifo");
        {
            let _guard = script_lock();
            let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
            assert!(status.success(), "mkfifo failed: {}", status);
        }

        let result = bounded_file_reading(&fifo);

        assert!(matches!(&result, Err(Error::NotRegularFile(p)) if *p == fifo), "{:?}", result);
    }

    #[test]
    fn file_that_is_a_directory_is_error() {
        let dir = TempDir::new();
        let result = bounded_file_reading(&dir.0);

        assert!(matches!(&result, Err(Error::NotRegularFile(p)) if *p == dir.0), "{:?}", result);
    }

    #[test]
    fn file_not_regular_error_names_the_path() {
        let error = Error::NotRegularFile(PathBuf::from("/run/temp-fifo"));

        assert_eq!(error.to_string(), r#"Not a regular file: "/run/temp-fifo""#);
    }

    fn assert_nvidia_bad_data(stdout: &str) {
        let result = parse_nvidia_output(GPU, stdout.as_bytes());

        assert!(matches!(&result,
            Err(Error::NvidiaBadData { gpu, output }) if gpu == GPU && output == stdout),
            "{:?}", result);
    }

    #[test]
    fn nvidia_temperature_line() {
        assert_eq!(parse_nvidia_output(GPU, b"67\n").unwrap(), 67);
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
        let message = parse_nvidia_output(GPU, b"[N/A]\n").unwrap_err().to_string();

        assert_eq!(message, r#"nvidia-smi reported bad data for GPU: 00000000:81:00.0: "[N/A]""#);
    }

    #[test]
    fn nvidia_record_with_or_without_one_line_ending_gives_reading() {
        for stdout in ["67", "67\n", "67\r\n"] {
            assert_eq!(parse_nvidia_output(GPU, stdout.as_bytes()).unwrap(), 67, "{:?}", stdout);
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
        let result = parse_nvidia_output(GPU, b"300\n");

        assert!(matches!(result, Err(Error::ReadingExceedsBounds)), "{:?}", result);
    }

    /// Limit for a script run that must finish, far above any scheduling delay
    const FINISH_LIMIT: Duration = Duration::from_secs(10);

    /// Limit for a script run that must time out
    const TIMEOUT_LIMIT: Duration = Duration::from_secs(1);

    /// Time a bounded call may take beyond its bound on a loaded host
    const RETURN_SLACK: Duration = Duration::from_secs(5);

    /// A fake program and the outcome of reading it
    struct ScriptRun {
        script: PathBuf,
        result: Result<u8, Error>,
        /// Time of the read alone, without the wait for the script lock
        elapsed: Duration,
        /// The runner that ran the script
        runner: CommandRunner,
    }

    /// Run `body` as a fake program `name` in `dir` and read it with `read`, on
    /// a runner of its own, as the zone loop reads a source
    fn script_reading(
        dir: &TempDir,
        name: &str,
        body: &str,
        read: impl FnOnce(&mut CommandRunner, &Path) -> Result<u8, Error>,
    ) -> ScriptRun {
        let _guard = script_lock();
        let script = write_script(dir, name, body);
        let mut runner = CommandRunner::default();

        let start = Instant::now();
        let result = blocking(|| read(&mut runner, &script));
        ScriptRun { script, result, elapsed: start.elapsed(), runner }
    }

    /// Run `body` as a fake nvidia-smi script in `dir`
    fn nvidia_script_reading(dir: &TempDir, body: &str, limit: Duration) -> ScriptRun {
        script_reading(dir, "nvidia-smi", body, |runner, script| read_nvidia_with_limit(runner, script, GPU, limit))
    }

    /// Run `body` as a fake smartctl script in `dir`
    fn smart_script_reading(dir: &TempDir, body: &str, limit: Duration) -> ScriptRun {
        script_reading(dir, "smartctl", body,
            |runner, script| read_smart_with_limit(runner, script, Path::new(BLOCK_DEV), limit))
    }

    /// Run `body` as a fake hdparm script in `dir`
    fn hdparm_script_reading(dir: &TempDir, body: &str, limit: Duration) -> ScriptRun {
        script_reading(dir, "hdparm", body,
            |runner, script| read_hdparm_with_limit(runner, script, Path::new(BLOCK_DEV), limit))
    }

    /// Assert that the run timed out. The error names the quoted script and
    /// then its arguments.
    fn assert_timed_out(run: &ScriptRun) {
        let program = format!("{:?} ", run.script);

        assert!(matches!(&run.result,
            Err(Error::CommandTimeout { command, .. }) if command.starts_with(&program)),
            "{:?}", run.result);
    }

    /// Assert that a script that recorded its pid in `pid_file` and then slept
    /// timed out within its bound, and that its process is gone. A script
    /// scheduled late may be killed before it records its pid; the runner then
    /// still shows that its child exited.
    fn assert_timed_out_and_killed(run: &mut ScriptRun, pid_file: &Path) {
        assert_timed_out(run);
        assert!(run.elapsed < TIMEOUT_LIMIT + KILL_GRACE + RETURN_SLACK, "returned after {:?}", run.elapsed);
        assert!(poll_until(|| run.runner.release_exited().then_some(())).is_some(),
            "the killed child is still kept");

        if let Some(pid) = written_pid(pid_file) {
            assert!(wait_until_gone(&pid), "pid {} still exists", pid);
        }
    }

    #[test]
    fn smart_script_output_gives_reading() {
        let dir = TempDir::new();
        let body = format!("[ \"$*\" = \"-j -A -n standby {}\" ] || exit 3\necho '{}'", BLOCK_DEV, SMART_OUTPUT);
        let run = smart_script_reading(&dir, &body, FINISH_LIMIT);

        assert_eq!(run.result.unwrap(), 41);
    }

    #[test]
    fn smart_script_output_longer_than_nvidia_cap_gives_reading() {
        let dir = TempDir::new();
        // smartctl's full attribute table is often longer than 4096 bytes
        let body = r#"printf '{ "pad": "%08000d", "temperature": { "current": 41 } }\n' 0"#;
        let run = smart_script_reading(&dir, body, FINISH_LIMIT);

        assert_eq!(run.result.unwrap(), 41);
    }

    #[test]
    fn smart_script_non_utf8_output_fails_to_parse() {
        let dir = TempDir::new();
        let body = r#"printf '{ "temperature": { "current": 41 }, "name": "\377" }\n'"#;
        let run = smart_script_reading(&dir, body, FINISH_LIMIT);

        assert!(matches!(&run.result,
            Err(Error::SmartParse { block_dev, .. }) if block_dev == Path::new(BLOCK_DEV)),
            "{:?}", run.result);
    }

    #[test]
    fn smart_script_timeout_kills_child() {
        let dir = TempDir::new();
        let pid_file = dir.0.join("pid");
        let mut run = smart_script_reading(&dir, &sleeping_body(&pid_file), TIMEOUT_LIMIT);

        assert_timed_out_and_killed(&mut run, &pid_file);
    }

    #[test]
    fn hdparm_script_output_gives_reading() {
        let dir = TempDir::new();
        let body = format!(
            "[ \"$*\" = \"-H {}\" ] || exit 3\nprintf '\\n{}:\\n drive temperature (celsius) is:  33\\n'",
            BLOCK_DEV, BLOCK_DEV);
        let run = hdparm_script_reading(&dir, &body, FINISH_LIMIT);

        assert_eq!(run.result.unwrap(), 33);
    }

    #[test]
    fn hdparm_script_non_utf8_after_temperature_line_gives_reading() {
        let dir = TempDir::new();
        let body = "printf '\\n/dev/sda:\\n drive temperature (celsius) is:  33\\n\\377\\376\\n'";
        let run = hdparm_script_reading(&dir, body, FINISH_LIMIT);

        assert_eq!(run.result.unwrap(), 33);
    }

    #[test]
    fn hdparm_script_failure_is_command_error() {
        let dir = TempDir::new();
        let run = hdparm_script_reading(&dir, "exit 9", FINISH_LIMIT);

        assert!(matches!(&run.result,
            Err(Error::Command { command, status }) if *command == run.script && status.code() == Some(9)),
            "{:?}", run.result);
    }

    #[test]
    fn hdparm_script_timeout_kills_child() {
        let dir = TempDir::new();
        let pid_file = dir.0.join("pid");
        let mut run = hdparm_script_reading(&dir, &sleeping_body(&pid_file), TIMEOUT_LIMIT);

        assert_timed_out_and_killed(&mut run, &pid_file);
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

    #[test]
    fn nvidia_script_non_utf8_output_is_bad_data() {
        let dir = TempDir::new();
        let run = nvidia_script_reading(&dir, "printf '7\\377\\n'", FINISH_LIMIT);

        assert!(matches!(&run.result,
            Err(Error::NvidiaBadData { gpu, output }) if gpu == GPU && output == "7\u{FFFD}\n"),
            "{:?}", run.result);
    }

    fn assert_output_over_cap(run: &ScriptRun) {
        assert!(matches!(&run.result,
            Err(Error::Io { path, source })
                if *path == run.script
                    && source.kind() == io::ErrorKind::InvalidData
                    && source.to_string().contains(&NVIDIA_OUTPUT_CAP.to_string())),
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
        let mut run = nvidia_script_reading(&dir, &descendant_body(&pid_file), READY_LIMIT);

        assert_timed_out(&run);
        assert!(run.elapsed < READY_LIMIT + KILL_GRACE + RETURN_SLACK, "returned after {:?}", run.elapsed);
        assert!(run.runner.release_exited(), "the exited script is still kept");
        assert!(written_pid(&pid_file).is_some(), "the script did not record its descendant's pid");
    }

    #[test]
    fn nvidia_script_timeout_kills_child() {
        let dir = TempDir::new();
        let pid_file = dir.0.join("pid");
        let mut run = nvidia_script_reading(&dir, &sleeping_body(&pid_file), TIMEOUT_LIMIT);

        assert_timed_out_and_killed(&mut run, &pid_file);
    }

    #[test]
    fn kept_child_blocks_only_the_program_of_its_own_source() {
        let dir = TempDir::new();
        let marker = dir.0.join("ran");
        let _guard = script_lock();
        let script = write_script(&dir, "tool", &format!("touch '{}'", marker.display()));
        let limits = RunLimits { time: FINISH_LIMIT, output: NVIDIA_OUTPUT_CAP };
        let mut readers = ZoneReaders::default();

        // Dropped inside the runtime, so the kept child is killed and reaped there
        blocking(move || {
            readers.runner(0).keep(spawn_sleeper(), &script);

            let blocked = readers.runner(0).run(&script, &[], limits);
            assert!(matches!(&blocked, Err(Error::CommandStillRunning { .. })), "{:?}", blocked);
            assert!(!marker.exists(), "source 0 started its program beside its kept child");

            let (status, _) = readers.runner(1).run(&script, &[], limits).unwrap();
            assert!(status.success());
            assert!(marker.exists(), "source 1 did not run its program");
        });
    }

    /// A block device that does not exist, so that a host that has the real
    /// disk tool touches no disk
    const ABSENT_DEV: &str = "/nonexistent/ipmi-fan-control-test-disk";

    /// Keep a child on the runner of source 0 as a run of `program`, the
    /// program that [`ZoneReaders::read`] runs for `source`, and give the
    /// results of reading `source` at index 0 and then at index 1
    fn reads_beside_kept_child(program: &str, source: &Source) -> (Result<u8, Error>, Result<u8, Error>) {
        let mut readers = ZoneReaders::default();

        // Dropped inside the runtime, so the kept child is killed and reaped there
        blocking(move || {
            readers.runner(0).keep(spawn_sleeper(), Path::new(program));

            (readers.read(0, source), readers.read(1, source))
        })
    }

    /// Assert that only the source whose runner keeps a child is refused. The
    /// other may fail another way, as the real program can be absent.
    fn assert_only_source_0_refused(program: &str, source: Source) {
        let (first, second) = reads_beside_kept_child(program, &source);

        assert!(matches!(&first, Err(Error::CommandStillRunning { .. })), "{:?}", first);
        assert!(!matches!(&second, Err(Error::CommandStillRunning { .. })), "{:?}", second);
    }

    #[test]
    fn nvidia_read_uses_the_runner_of_its_own_index() {
        assert_only_source_0_refused("nvidia-smi", Source::Nvidia { gpu: GPU.into() });
    }

    #[test]
    fn smart_read_uses_the_runner_of_its_own_index() {
        assert_only_source_0_refused("smartctl", Source::Smart { block_dev: ABSENT_DEV.into() });
    }

    #[test]
    fn hdparm_read_uses_the_runner_of_its_own_index() {
        assert_only_source_0_refused("hdparm", Source::Hdparm { block_dev: ABSENT_DEV.into() });
    }

    #[test]
    fn millidegrees_round_up_to_whole_degrees() {
        assert_eq!(millidegrees_to_celsius(45500), Some(46));
        assert_eq!(millidegrees_to_celsius(45001), Some(46));
        assert_eq!(millidegrees_to_celsius(45000), Some(45));
        assert_eq!(millidegrees_to_celsius(0), Some(0));
    }

    #[test]
    fn millidegrees_clamp_above_and_reject_below_zero() {
        assert_eq!(millidegrees_to_celsius(300000), Some(255));
        assert_eq!(millidegrees_to_celsius(i64::MAX), Some(255));
        assert_eq!(millidegrees_to_celsius(-1), None);
        assert_eq!(millidegrees_to_celsius(-1000), None);
        assert_eq!(millidegrees_to_celsius(i64::MIN), None);
    }

    /// A hwmon tree with chip `k10temp` at `hwmon3`, whose `temp1_input`
    /// holds `millidegrees`
    fn k10temp_tree(millidegrees: &str) -> TempDir {
        let dir = TempDir::new();
        let chip = dir.0.join("hwmon3");
        fs::create_dir(&chip).unwrap();
        fs::write(chip.join("name"), "k10temp\n").unwrap();
        fs::write(chip.join("temp1_input"), millidegrees).unwrap();
        dir
    }

    #[test]
    fn hwmon_source_reads_its_chip_input() {
        let dir = k10temp_tree("45500\n");
        let mut readers = ZoneReaders::new(HwmonRoot::new(dir.0.clone()));

        let reading = readers.read(0, &Source::Hwmon { chip: "k10temp".into(), input: 1 });

        assert_eq!(reading.unwrap(), 46);
    }

    #[test]
    fn hwmon_source_fails_for_an_absent_input() {
        let dir = k10temp_tree("45500\n");
        let mut readers = ZoneReaders::new(HwmonRoot::new(dir.0.clone()));

        let reading = readers.read(0, &Source::Hwmon { chip: "k10temp".into(), input: 2 });

        assert!(matches!(&reading, Err(Error::Io { path, .. }) if path.ends_with("hwmon3/temp2_input")), "{:?}", reading);
    }

    fn k10temp_reading_with_fault(fault: &str) -> Result<u8, Error> {
        let dir = k10temp_tree("45500\n");
        fs::write(dir.0.join("hwmon3/temp1_fault"), fault).unwrap();
        let mut readers = ZoneReaders::new(HwmonRoot::new(dir.0.clone()));

        readers.read(0, &Source::Hwmon { chip: "k10temp".into(), input: 1 })
    }

    #[test]
    fn hwmon_source_fails_when_its_input_reports_a_fault() {
        let reading = k10temp_reading_with_fault("1\n");

        assert!(matches!(&reading, Err(Error::HwmonFault { path }) if path.ends_with("hwmon3/temp1_fault")), "{:?}", reading);
    }

    #[test]
    fn hwmon_source_fails_when_its_fault_file_does_not_parse() {
        let reading = k10temp_reading_with_fault("garbage\n");

        assert!(matches!(&reading, Err(Error::HwmonParse { path, .. }) if path.ends_with("hwmon3/temp1_fault")), "{:?}", reading);
    }

    #[test]
    fn hwmon_source_reads_when_its_input_reports_no_fault() {
        assert_eq!(k10temp_reading_with_fault("0\n").unwrap(), 46);
    }

    #[test]
    fn hwmon_source_fails_for_a_negative_reading() {
        // An open thermistor on nct67xx chips reads -128 °C
        let dir = k10temp_tree("-128000\n");
        let mut readers = ZoneReaders::new(HwmonRoot::new(dir.0.clone()));

        let reading = readers.read(0, &Source::Hwmon { chip: "k10temp".into(), input: 1 });

        let error = reading.err().expect("a negative reading fails");
        assert!(matches!(&error, Error::HwmonValueOutOfRange { path, value: -128000 }
                if path.ends_with("hwmon3/temp1_input")), "{:?}", error);
        let text = error.to_string();
        assert!(text.contains("temp1_input") && text.contains("-128000"), "{}", text);
    }

    #[test]
    fn runner_of_a_source_persists_between_reads() {
        let source = Source::Nvidia { gpu: GPU.into() };
        let mut readers = ZoneReaders::default();

        let (first, second) = blocking(move || {
            readers.runner(0).keep(spawn_sleeper(), Path::new("nvidia-smi"));

            (readers.read(0, &source), readers.read(0, &source))
        });

        assert!(matches!(&first, Err(Error::CommandStillRunning { .. })), "{:?}", first);
        assert!(matches!(&second, Err(Error::CommandStillRunning { .. })), "{:?}", second);
    }

    /// A zone with no wait between retries
    fn zone(sources: Vec<SourceConfig>, retries: usize) -> Zone {
        Zone {
            session: SessionName::default(),
            interval: Interval::default(),
            retries: Retries(retries),
            retry_delay_ms: RetryDelayMs(0),
            ipmi_zones: Some(vec![0]),
            sources,
            aggregation: Aggregation::Maximum,
            steps: Some(vec![Step { temp: 45, dcycle: 45 }, Step { temp: 75, dcycle: 100 }]),
            curve: None,
            duty_aggregation: Aggregation::Maximum,
            hysteresis: 0,
            curve_hysteresis: None,
            fans: None,
            fan_list: Vec::new(),
        }
    }

    fn file(path: &str, on_failure: OnFailure) -> SourceConfig {
        SourceConfig { on_failure, ..SourceConfig::new(Source::File { path: path.to_owned() }) }
    }

    fn ipmi(sensor: &str, on_failure: OnFailure) -> SourceConfig {
        SourceConfig { on_failure, ..SourceConfig::new(Source::Ipmi { sensor: sensor.to_owned() }) }
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
        let result = read_zone_sources(zone, failed_last, &mut query, &mut |_, s| reader.read(s));

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
        let readings = read_zone_sources(zone, &failed_last, &mut || Ok(HashMap::new()), &mut |_, s| reader.read(s))
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

        let readings = read_zone_sources(&zone, &[true], &mut query, &mut |_, _| panic!("no file source")).unwrap();

        assert!(matches!(&readings[..], [Failed(_)]), "{:?}", readings);
        assert_eq!(queries, 1);
    }

    #[test]
    fn sensor_failed_last_poll_takes_value_of_retry_for_another_sensor() {
        let zone = zone(vec![ipmi("GPU Temp", IGNORE), ipmi(CPU, IGNORE)], 2);
        let maps = [sensor_map(&[]), sensor_map(&[("GPU Temp", celsius(60)), (CPU, celsius(50))])];
        let mut queries = 0;

        let readings = read_zone_sources(&zone, &[true, false], &mut changing_query(&maps, &mut queries),
            &mut |_, _| panic!("no file source")).unwrap();

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
        let mut reader = |_: usize, _: &Source| {
            events.borrow_mut().push("disk");
            Ok(35)
        };

        let readings = read_zone_sources(&zone, &[false, false], &mut query, &mut reader).unwrap();

        assert_eq!(readings, [Temp(50), Temp(35)]);
        assert_eq!(events.into_inner(), ["disk", "query"]);
    }

    #[test]
    fn reader_is_given_the_index_of_each_source() {
        let zone = zone(vec![ipmi(CPU, IGNORE), file("gpu", IGNORE), file("disk", IGNORE)], 1);
        let reader = FakeReader::new(&[("gpu", 1, 70), ("disk", 0, 35)]);
        let mut calls = Vec::new();
        let mut query = || Ok(sensor_map(&[(CPU, celsius(50))]));

        let readings = read_zone_sources(&zone, &[false, false, false], &mut query, &mut |index, source| {
            calls.push(index);
            reader.read(source)
        }).unwrap();

        assert_eq!(readings, [Temp(50), Temp(70), Temp(35)]);
        assert_eq!(calls, [1, 1, 2]);
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
            &mut |_, s| reader.read(s)).unwrap();

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
            &mut |_, _| panic!("no file source")).unwrap();

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

        let result = read_zone_sources(&zone, &[false, false], &mut query, &mut |_, _| panic!("no file source"));

        assert_retries_failed(&result, 3, "\"(ipmi)\": BMC busy");
        assert_eq!(queries, 3);
    }

    #[test]
    fn failed_query_gives_failed_reading_to_each_ignore_sensor() {
        let zone = zone(vec![ipmi("GPU Temp", IGNORE), ipmi(CPU, OnFailure::Dcycle { dcycle: 80 }), file("disk", IGNORE)], 1);
        let reader = FakeReader::new(&[("disk", 0, 35)]);
        let mut query = || Err(query_error());

        let readings = read_zone_sources(&zone, &[false, false, false], &mut query, &mut |_, s| reader.read(s)).unwrap();
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

        let readings = read_zone_sources(&zone, &[false], &mut query, &mut |_, _| panic!("no file source")).unwrap();

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
