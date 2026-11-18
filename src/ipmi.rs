use {
    std::{
        collections::{HashMap, HashSet},
        env,
        result,
    },
    log::{trace, warn},
    crate::{
        bindings,
        config::SessionType,
        freeipmi::{self, LfiSession, LimSession, SensorReading},
    },
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    FreeIpmi(#[from] freeipmi::Error),
    #[error("Expected response to be {expected} bytes, but have {actual} bytes")]
    BadResponseSize {
        expected: usize,
        actual: usize,
    }
}

type Result<T, E = Error> = result::Result<T, E>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FanMode {
    Standard,       // 0
    Full,           // 1
    Optimal,        // 2
    HeavyIo,        // 4
    Unknown(u8),    // Anything else
}

impl From<u8> for FanMode {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::Standard,
            1 => Self::Full,
            2 => Self::Optimal,
            4 => Self::HeavyIo,
            n => Self::Unknown(n),
        }
    }
}

impl From<FanMode> for u8 {
    fn from(mode: FanMode) -> Self {
        match mode {
            FanMode::Standard => 0,
            FanMode::Full => 1,
            FanMode::Optimal => 2,
            FanMode::HeavyIo => 4,
            FanMode::Unknown(n) => n,
        }
    }
}

const NET_FN_GENERIC: u8 = bindings::IPMI_NET_FN_OEM_SUPERMICRO_GENERIC_RQ as u8;
const CMD_FAN_MODE: u8 = 0x45;
const CMD_GENERIC_EXT: u8 = bindings::IPMI_CMD_OEM_SUPERMICRO_GENERIC_EXTENSION as u8;
const DATA_DUTY_CYCLE: u8 = 0x66;
const DATA_ACTION_READ: u8 = 0x0;
const DATA_ACTION_WRITE: u8 = 0x1;

// libipmimonitoring doesn't expose its underlying session and there's no way to
// give it an existing session, so we're stuck creating two connections.
pub struct Ipmi {
    lfi: LfiSession,
    lim: LimSession,
}

impl Ipmi {
    /// Createt an [`Ipmi`] instance for the given session type.
    pub fn new(st: &SessionType) -> Result<Self> {
        let lfi = LfiSession::new(st)?;
        let mut lim = LimSession::new(st)?;

        let temp_dir = env::temp_dir();
        trace!("SDR cache directory: {:?}", temp_dir);

        lim.set_sdr_cache_directory(&temp_dir)?;
        // This call is required, even if we're not loading a file
        lim.set_sensor_config_file(None)?;

        Ok(Self { lfi, lim })
    }

    /// Execute raw IPMI command and return the output. The output does not
    /// include the command number nor the status. If the command does not
    /// return a successful response or if the size of the response does not
    /// match the specified value, an error is returned.
    fn execute(
        &mut self,
        net_fn: u8,
        command: u8,
        data: &[u8],
        expected_size: usize,
    ) -> Result<Vec<u8>> {
        trace!("Running IPMI command: net_fn={:02x}, command={:02x}, data={:02x?}",
               net_fn, command, data);

        let response = self.lfi.raw_command(net_fn, command, data)?;

        if response.len() != expected_size {
            return Err(Error::BadResponseSize {
                expected: expected_size,
                actual: response.len(),
            });
        }

        Ok(response)
    }

    /// Get the current fan mode.
    pub fn get_fan_mode(&mut self) -> Result<FanMode> {
        let response = self.execute(
            NET_FN_GENERIC,
            CMD_FAN_MODE,
            &[DATA_ACTION_READ],
            1,
        )?;

        Ok(FanMode::from(response[0]))
    }

    /// Set the fan mode.
    pub fn set_fan_mode(&mut self, mode: FanMode) -> Result<()> {
        self.execute(
            NET_FN_GENERIC,
            CMD_FAN_MODE,
            &[
                DATA_ACTION_WRITE,
                mode.into(),
            ],
            0,
        )?;

        Ok(())
    }

    /// Get the current duty cycle. The valud should be in the range [0, 100],
    /// but is not guaranteed as this function returns the raw value supplied by
    /// the BMC.
    pub fn get_duty_cycle(&mut self, zone: u8) -> Result<u8> {
        let response = self.execute(
            NET_FN_GENERIC,
            CMD_GENERIC_EXT,
            &[
                DATA_DUTY_CYCLE,
                DATA_ACTION_READ,
                zone,
            ],
            1,
        )?;

        Ok(response[0])
    }

    /// Set the duty cycle. The valud should be in the range [0, 100], but this
    /// is not validated. The raw `dcycle` value will be sent to the BMC as-is.
    pub fn set_duty_cycle(&mut self, zone: u8, dcycle: u8) -> Result<()> {
        self.execute(
            NET_FN_GENERIC,
            CMD_GENERIC_EXT,
            &[
                DATA_DUTY_CYCLE,
                DATA_ACTION_WRITE,
                zone,
                dcycle,
            ],
            0,
        )?;

        Ok(())
    }

    /// Get readings for all temperature sensors. If the query or the iteration
    /// fails, no partial results will be returned. A sensor whose name or
    /// reading cannot be read is left out, and a sensor with no reading maps to
    /// [`None`], as [`collect_sensor_readings`] describes for sensors that
    /// share a name.
    pub fn get_temperature_readings(&mut self)
        -> Result<HashMap<String, Option<SensorReading>>> {
        read_all_sensors(&mut self.lim)
    }
}

/// The operations of one sensor reading iteration, so that the traversal can be
/// tested without a BMC
trait SensorIterator {
    /// Start the query and return the number of entries
    fn start_query(&mut self) -> result::Result<usize, freeipmi::Error>;
    /// Read the name of the current entry
    fn current_name(&mut self) -> result::Result<String, freeipmi::Error>;
    /// Read the reading of the current entry
    fn current_reading(&mut self) -> result::Result<Option<SensorReading>, freeipmi::Error>;
    /// Move to the next entry
    fn advance(&mut self) -> result::Result<(), freeipmi::Error>;
}

impl SensorIterator for LimSession {
    fn start_query(&mut self) -> result::Result<usize, freeipmi::Error> {
        self.temperature_sensor_readings()
    }

    fn current_name(&mut self) -> result::Result<String, freeipmi::Error> {
        self.read_sensor_name()
    }

    fn current_reading(&mut self) -> result::Result<Option<SensorReading>, freeipmi::Error> {
        self.read_sensor()
    }

    fn advance(&mut self) -> result::Result<(), freeipmi::Error> {
        self.iterator_next()
    }
}

/// Run one sensor reading iteration. An entry whose name or reading failed
/// still advances the iterator and is handled by [`collect_sensor_readings`].
/// A failed query or advance returns the error and no readings.
fn read_all_sensors(sensors: &mut impl SensorIterator)
    -> Result<HashMap<String, Option<SensorReading>>> {
    let num_sensors = sensors.start_query()?;
    trace!("Number of sensors: {}", num_sensors);

    let mut entries = Vec::with_capacity(num_sensors);

    for _ in 0..num_sensors {
        entries.push((sensors.current_name(), sensors.current_reading()));

        sensors.advance()?;
    }

    Ok(collect_sensor_readings(entries))
}

/// The name and the reading of one sensor of a sensor reading iteration
type SensorEntry = (
    result::Result<String, freeipmi::Error>,
    result::Result<Option<SensorReading>, freeipmi::Error>,
);

/// Build the map of sensor readings from the entries of one iteration, so that
/// one bad sensor does not discard the others. FreeIPMI does not deduplicate
/// sensor names, so the outcome for a name depends on every entry with that
/// name, whatever their order:
///
/// - If any entry failed to read its reading, the name is left out with a
///   warning, and a source that needs it fails as if the sensor were missing.
/// - Otherwise, if any entry has no reading, the name maps to [`None`].
/// - Otherwise, of two real readings, the last one wins.
///
/// An entry whose name failed to read is left out with a warning. It cannot be
/// matched to a name, so it does not affect any entry that has one.
fn collect_sensor_readings(entries: impl IntoIterator<Item = SensorEntry>)
    -> HashMap<String, Option<SensorReading>> {
    let mut result = HashMap::new();
    let mut no_reading_names = HashSet::new();
    let mut failed_names = HashSet::new();

    for (name, reading) in entries {
        match (name, reading) {
            (Ok(name), Ok(None)) => {
                no_reading_names.insert(name);
            }
            (Ok(name), Ok(reading)) => {
                result.insert(name, reading);
            }
            (Ok(name), Err(e)) => {
                warn!("Skipping IPMI sensor {:?}: failed to read its reading: {}", name, e);
                failed_names.insert(name);
            }
            (Err(e), _) => {
                warn!("Skipping an IPMI sensor: failed to read its name: {}", e);
            }
        }
    }

    // A real reading of a duplicate must not hide that a sensor of the same
    // name has none, nor that one failed. The removal runs last because a
    // failure outranks a missing reading.
    for name in no_reading_names {
        result.insert(name, None);
    }

    for name in &failed_names {
        result.remove(name);
    }

    result
}

#[cfg(test)]
mod tests {
    use {
        std::collections::HashMap,
        super::{collect_sensor_readings, read_all_sensors, Error, SensorEntry, SensorIterator},
        crate::freeipmi::{self, SensorReading, SensorUnits, SensorValue},
    };

    fn celsius(temp: u32) -> Option<SensorReading> {
        Some(SensorReading { value: SensorValue::Uint32(temp), units: SensorUnits::Celsius })
    }

    fn lim_error(action: &'static str) -> freeipmi::Error {
        freeipmi::Error::Lim { action, message: "sensor reading failed" }
    }

    fn good(name: &str, reading: Option<SensorReading>) -> SensorEntry {
        (Ok(name.to_owned()), Ok(reading))
    }

    /// The temperature of each sensor in `readings`, or `None` for a sensor
    /// without a reading
    fn temps(readings: &HashMap<String, Option<SensorReading>>) -> HashMap<&str, Option<u32>> {
        readings.iter()
            .map(|(name, reading)| {
                let temp = reading.map(|r| match r.value {
                    SensorValue::Uint32(t) => t,
                    other => panic!("unexpected value {:?}", other),
                });
                (name.as_str(), temp)
            })
            .collect()
    }

    #[test]
    fn all_good_entries_are_kept() {
        let readings = collect_sensor_readings(vec![
            good("CPU Temp", celsius(45)),
            good("System Temp", celsius(30)),
            good("Peripheral Temp", None),
        ]);

        assert_eq!(temps(&readings), HashMap::from([
            ("CPU Temp", Some(45)),
            ("System Temp", Some(30)),
            ("Peripheral Temp", None),
        ]));
    }

    #[test]
    fn entry_with_bad_reading_is_skipped_and_good_ones_are_kept() {
        let readings = collect_sensor_readings(vec![
            good("CPU Temp", celsius(45)),
            (Ok("VRM Temp".to_owned()), Err(lim_error("read sensor units"))),
            good("System Temp", celsius(30)),
        ]);

        assert_eq!(temps(&readings), HashMap::from([
            ("CPU Temp", Some(45)),
            ("System Temp", Some(30)),
        ]));
    }

    #[test]
    fn entry_with_bad_name_is_skipped() {
        let readings = collect_sensor_readings(vec![
            (Err(lim_error("read sensor name")), Ok(celsius(99))),
            good("CPU Temp", celsius(45)),
            (Err(lim_error("read sensor name")), Err(lim_error("read sensor value type"))),
        ]);

        assert_eq!(temps(&readings), HashMap::from([("CPU Temp", Some(45))]));
    }

    #[test]
    fn name_that_failed_after_a_good_entry_is_absent() {
        let readings = collect_sensor_readings(vec![
            good("CPU Temp", celsius(28)),
            good("System Temp", celsius(30)),
            (Ok("CPU Temp".to_owned()), Err(lim_error("read sensor units"))),
        ]);

        assert_eq!(temps(&readings), HashMap::from([("System Temp", Some(30))]));
    }

    #[test]
    fn name_that_failed_before_a_good_entry_is_absent() {
        let readings = collect_sensor_readings(vec![
            (Ok("CPU Temp".to_owned()), Err(lim_error("read sensor units"))),
            good("System Temp", celsius(30)),
            good("CPU Temp", celsius(28)),
        ]);

        assert_eq!(temps(&readings), HashMap::from([("System Temp", Some(30))]));
    }

    #[test]
    fn name_without_a_reading_before_a_real_one_stays_none() {
        let readings = collect_sensor_readings(vec![
            good("CPU Temp", None),
            good("System Temp", celsius(30)),
            good("CPU Temp", celsius(28)),
        ]);

        assert_eq!(temps(&readings), HashMap::from([
            ("CPU Temp", None),
            ("System Temp", Some(30)),
        ]));
    }

    #[test]
    fn name_without_a_reading_after_a_real_one_stays_none() {
        let readings = collect_sensor_readings(vec![
            good("CPU Temp", celsius(28)),
            good("System Temp", celsius(30)),
            good("CPU Temp", None),
        ]);

        assert_eq!(temps(&readings), HashMap::from([
            ("CPU Temp", None),
            ("System Temp", Some(30)),
        ]));
    }

    #[test]
    fn name_without_a_reading_that_failed_later_is_absent() {
        let readings = collect_sensor_readings(vec![
            good("CPU Temp", None),
            good("System Temp", celsius(30)),
            (Ok("CPU Temp".to_owned()), Err(lim_error("read sensor units"))),
        ]);

        assert_eq!(temps(&readings), HashMap::from([("System Temp", Some(30))]));
    }

    #[test]
    fn name_that_failed_before_an_entry_without_a_reading_is_absent() {
        let readings = collect_sensor_readings(vec![
            (Ok("CPU Temp".to_owned()), Err(lim_error("read sensor units"))),
            good("System Temp", celsius(30)),
            good("CPU Temp", None),
        ]);

        assert_eq!(temps(&readings), HashMap::from([("System Temp", Some(30))]));
    }

    #[test]
    fn last_of_two_good_entries_with_one_name_wins() {
        let readings = collect_sensor_readings(vec![
            good("CPU Temp", celsius(28)),
            good("System Temp", celsius(30)),
            good("CPU Temp", celsius(45)),
        ]);

        assert_eq!(temps(&readings), HashMap::from([
            ("CPU Temp", Some(45)),
            ("System Temp", Some(30)),
        ]));
    }

    const QUERY_ACTION: &str = "get temperature sensor readings";
    const ADVANCE_ACTION: &str = "get next sensor reading";

    /// A sensor iteration over fixed entries that counts its advances. Reading
    /// past the last entry panics.
    struct FakeSensors {
        entries: Vec<SensorEntry>,
        query_fails: bool,
        failing_advance: Option<usize>,
        position: usize,
        advances: usize,
    }

    impl FakeSensors {
        fn new(entries: Vec<SensorEntry>) -> Self {
            Self { entries, query_fails: false, failing_advance: None, position: 0, advances: 0 }
        }
    }

    impl SensorIterator for FakeSensors {
        fn start_query(&mut self) -> Result<usize, freeipmi::Error> {
            if self.query_fails {
                return Err(lim_error(QUERY_ACTION));
            }

            Ok(self.entries.len())
        }

        fn current_name(&mut self) -> Result<String, freeipmi::Error> {
            self.entries[self.position].0.as_ref()
                .cloned()
                .map_err(|_| lim_error("read sensor name"))
        }

        fn current_reading(&mut self) -> Result<Option<SensorReading>, freeipmi::Error> {
            self.entries[self.position].1.as_ref()
                .copied()
                .map_err(|_| lim_error("read sensor units"))
        }

        fn advance(&mut self) -> Result<(), freeipmi::Error> {
            self.advances += 1;

            if self.failing_advance == Some(self.position) {
                return Err(lim_error(ADVANCE_ACTION));
            }

            self.position += 1;
            Ok(())
        }
    }

    /// The freeipmi error inside `result`, which must be an error
    fn freeipmi_error(result: super::Result<HashMap<String, Option<SensorReading>>>) -> freeipmi::Error {
        match result {
            Err(Error::FreeIpmi(e)) => e,
            other => panic!("expected a freeipmi error, got {:?}", other),
        }
    }

    #[test]
    fn traversal_of_good_entries_gives_the_full_map() {
        let mut sensors = FakeSensors::new(vec![
            good("CPU Temp", celsius(45)),
            good("System Temp", celsius(30)),
            good("Peripheral Temp", None),
        ]);

        let readings = read_all_sensors(&mut sensors).unwrap();

        assert_eq!(temps(&readings), HashMap::from([
            ("CPU Temp", Some(45)),
            ("System Temp", Some(30)),
            ("Peripheral Temp", None),
        ]));
        assert_eq!(sensors.advances, 3);
    }

    #[test]
    fn traversal_advances_once_per_entry_past_bad_entries() {
        let mut sensors = FakeSensors::new(vec![
            good("CPU Temp", celsius(45)),
            (Err(lim_error("read sensor name")), Ok(celsius(99))),
            (Ok("VRM Temp".to_owned()), Err(lim_error("read sensor units"))),
            good("System Temp", celsius(30)),
        ]);

        let readings = read_all_sensors(&mut sensors).unwrap();

        assert_eq!(sensors.advances, 4);
        assert_eq!(temps(&readings), HashMap::from([
            ("CPU Temp", Some(45)),
            ("System Temp", Some(30)),
        ]));
    }

    #[test]
    fn traversal_returns_the_advance_error_and_no_map() {
        let mut sensors = FakeSensors::new(vec![
            good("CPU Temp", celsius(45)),
            good("System Temp", celsius(30)),
            good("Peripheral Temp", celsius(35)),
        ]);
        sensors.failing_advance = Some(1);

        let error = freeipmi_error(read_all_sensors(&mut sensors));

        assert_eq!(error, lim_error(ADVANCE_ACTION));
        assert_eq!(sensors.advances, 2);
    }

    #[test]
    fn traversal_returns_the_query_error() {
        let mut sensors = FakeSensors::new(vec![good("CPU Temp", celsius(45))]);
        sensors.query_fails = true;

        let error = freeipmi_error(read_all_sensors(&mut sensors));

        assert_eq!(error, lim_error(QUERY_ACTION));
        assert_eq!(sensors.advances, 0);
    }
}
