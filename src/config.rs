use {
    std::{
        collections::HashMap,
        fmt,
        fs,
        path::Path,
        time::Duration,
    },
    clap::{Parser, ValueEnum},
    retry::delay::Fixed,
    serde::{
        de::{
            self,
            value::{MapAccessDeserializer, SeqAccessDeserializer},
            MapAccess,
            SeqAccess,
            Visitor,
        },
        Deserialize,
        Deserializer,
    },
    crate::error::{Error, Result},
};

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Error => f.write_str("error"),
            Self::Warn => f.write_str("warn"),
            Self::Info => f.write_str("info"),
            Self::Debug => f.write_str("debug"),
            Self::Trace => f.write_str("trace"),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct Interval(pub u16);

impl Interval {
    pub fn to_duration(self) -> Duration {
        Duration::from_secs(self.0.into())
    }
}

impl Default for Interval {
    fn default() -> Self {
        Self(1)
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct Retries(pub usize);

impl Default for Retries {
    fn default() -> Self {
        Self(2)
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct RetryDelayMs(pub u64);

impl RetryDelayMs {
    pub fn to_fixed(self) -> Fixed {
        Fixed::from_millis(self.0)
    }
}

impl Default for RetryDelayMs {
    fn default() -> Self {
        Self(500)
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub temp: u8,
    pub dcycle: u8,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SessionName(pub String);

impl Default for SessionName {
    fn default() -> Self {
        Self("default".to_owned())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "lowercase", tag = "type")]
pub enum Source {
    Ipmi {
        sensor: String,
    },
    File {
        // TOML can't encode OsString
        path: String,
    },
    Smart {
        // TOML can't encode OsString
        block_dev: String,
    },
    Hdparm {
        // TOML can't encode OsString
        block_dev: String,
    },
    Nvidia {
        // PCI bus id or GPU UUID, as nvidia-smi -i accepts
        gpu: String,
    },
}

/// What a zone does when a source has failed every attempt of a poll
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "lowercase", tag = "type")]
pub enum OnFailure {
    /// Stop the daemon, which sets every zone to 100%
    // Serde ignores extra keys on a unit variant of a tagged enum; an empty
    // struct variant rejects them
    Exit {},
    /// Leave the source out until it reads again
    Ignore {},
    /// Leave the source out and keep the zone at `dcycle` or above
    Dcycle {
        dcycle: u8,
    },
}

// `#[default]` needs a unit variant
impl Default for OnFailure {
    fn default() -> Self {
        Self::Exit {}
    }
}

/// A source with an optional curve of its own. A source with `steps` maps its
/// reading through them; a source without feeds the zone's curve.
#[derive(Clone, Debug)]
pub struct SourceConfig {
    pub kind: Source,
    pub steps: Option<Vec<Step>>,
    pub on_failure: OnFailure,
    pub smoothing_samples: u8,
    /// The name of an entry of `curves`, which sets `steps`
    pub curve: Option<String>,
    /// The hysteresis of the named curve that set `steps`; `None` means the
    /// zone's `hysteresis` applies. Not a config key.
    pub curve_hysteresis: Option<u8>,
}

impl SourceConfig {
    /// A source of `kind` with every other key at its default
    pub fn new(kind: Source) -> Self {
        Self {
            kind,
            steps: None,
            on_failure: OnFailure::default(),
            smoothing_samples: 1,
            curve: None,
            curve_hysteresis: None,
        }
    }
}

/// Serde cannot combine `flatten` with `deny_unknown_fields`, so `steps`,
/// `curve`, `on_failure` and `smoothing_samples` are taken out of the table by
/// hand and the rest goes to [`Source`], which still rejects an unknown key or type. A
/// sequence, the positional form that [`Source`] accepts, goes to it unchanged
/// and has every other key at its default.
impl<'de> Deserialize<'de> for SourceConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SourceConfigVisitor;

        impl<'de> Visitor<'de> for SourceConfigVisitor {
            type Value = SourceConfig;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a source table")
            }

            fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let kind = Source::deserialize(SeqAccessDeserializer::new(seq))?;

                Ok(SourceConfig::new(kind))
            }

            // The table is collected by hand, so a repeated key is rejected
            // here; collecting into a map would keep the last value
            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut table = toml::value::Table::new();
                let mut steps = None;
                let mut curve = None;
                let mut on_failure = None;
                let mut smoothing_samples = None;

                while let Some(key) = map.next_key::<String>()? {
                    let repeated = match key.as_str() {
                        "steps" => steps.is_some(),
                        "curve" => curve.is_some(),
                        "on_failure" => on_failure.is_some(),
                        "smoothing_samples" => smoothing_samples.is_some(),
                        _ => table.contains_key(&key),
                    };

                    if repeated {
                        return Err(de::Error::custom(format!("duplicate field `{}`", key)));
                    }

                    let value = map.next_value::<toml::Value>()?;

                    match key.as_str() {
                        "steps" => {
                            steps = Some(Vec::<Step>::deserialize(value)
                                .map_err(|e| de::Error::custom(format!("steps: {}", e)))?);
                        }
                        "curve" => {
                            curve = Some(String::deserialize(value)
                                .map_err(|e| de::Error::custom(format!("curve: {}", e)))?);
                        }
                        "on_failure" => {
                            on_failure = Some(OnFailure::deserialize(value)
                                .map_err(|e| de::Error::custom(format!("on_failure: {}", e)))?);
                        }
                        "smoothing_samples" => {
                            smoothing_samples = Some(u8::deserialize(value)
                                .map_err(|e| de::Error::custom(format!("smoothing_samples: {}", e)))?);
                        }
                        _ => {
                            table.insert(key, value);
                        }
                    }
                }

                let kind = Source::deserialize(toml::Value::Table(table))
                    .map_err(de::Error::custom)?;

                let defaults = SourceConfig::new(kind);

                Ok(SourceConfig {
                    steps,
                    curve,
                    on_failure: on_failure.unwrap_or(defaults.on_failure),
                    smoothing_samples: smoothing_samples.unwrap_or(defaults.smoothing_samples),
                    ..defaults
                })
            }
        }

        deserializer.deserialize_any(SourceConfigVisitor)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "lowercase", tag = "type")]
pub enum Aggregation {
    #[default]
    Maximum,
    Average {
        top: Option<usize>,
    },
}

/// A zone may also be written as a TOML array, whose values serde reads in the
/// order the fields are declared. Add a new field after the existing ones, so
/// that a config in that form keeps loading.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Zone {
    #[serde(default)]
    pub session: SessionName,
    #[serde(default)]
    pub interval: Interval,
    #[serde(default)]
    pub retries: Retries,
    #[serde(default)]
    pub retry_delay_ms: RetryDelayMs,
    pub ipmi_zones: Vec<u8>,
    pub sources: Vec<SourceConfig>,
    #[serde(default)]
    pub aggregation: Aggregation,
    /// The zone curve. Validation lets it be absent only when every source
    /// has `steps`, so the zone curve is never used.
    #[serde(default)]
    pub steps: Option<Vec<Step>>,
    /// How the duties of the zone's curves combine into one
    #[serde(default)]
    pub duty_aggregation: Aggregation,
    /// Degrees the temperature of an inline curve must fall before its duty
    /// follows
    #[serde(default)]
    pub hysteresis: u8,
    /// The name of an entry of `curves`, which sets `steps`
    #[serde(default)]
    pub curve: Option<String>,
    /// The hysteresis of the named curve that set `steps`; `None` means
    /// `hysteresis` applies. Not a config key.
    #[serde(skip)]
    pub curve_hysteresis: Option<u8>,
}

impl Zone {
    pub fn retry_iter(&self) -> impl Iterator<Item = Duration> {
        self.retry_delay_ms.to_fixed().take(self.retries.0)
    }
}

/// Simple wrapper around a password string with a redacted Debug implementation
#[derive(Deserialize)]
pub struct Password(pub String);

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "lowercase", tag = "type")]
pub enum SessionType {
    #[default]
    Local,
    Remote {
        hostname: String,
        username: String,
        password: Password,
    },
}

#[derive(Clone, Copy, Debug, Parser, ValueEnum)]
#[clap(rename_all = "lower")]
pub enum IpmitoolInterfaceOpt {
    LanPlus,
}

/// Basic compatibility layer for ipmitool's command line arguments
#[derive(Debug, Parser)]
pub struct IpmitoolOpt {
    #[clap(short = 'I', value_enum)]
    pub interface: IpmitoolInterfaceOpt,
    #[clap(short = 'H')]
    pub hostname: String,
    #[clap(short = 'U')]
    pub username: String,
    #[clap(short = 'P')]
    pub password: String,
}

#[derive(Debug, Default)]
pub struct SessionTypeCompat(pub SessionType);

/// Deserialize either a map as a native [`SessionType`] instance or an array of
/// strings as ipmitool arguments.
impl<'de> Deserialize<'de> for SessionTypeCompat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SessionTypeVisitor;

        impl<'de> Visitor<'de> for SessionTypeVisitor {
            type Value = SessionType;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("native session configuration or ipmitool compatibility layer arguments")
            }

            // Deserialize an array and parse it as ipmitool arguments
            fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let args = Vec::<String>::deserialize(SeqAccessDeserializer::new(seq))?;

                // ipmitool defaults to a local connection with run without arguments
                if args.is_empty() {
                    return Ok(SessionType::Local);
                }

                let argv0 = ["ipmitool_compat".to_owned()];
                let argv = argv0.iter().chain(args.iter());

                let opt = IpmitoolOpt::try_parse_from(argv)
                    .map_err(|e| de::Error::custom(format!("ipmitool compatibility layer: {}", e)))?;

                Ok(SessionType::Remote {
                    hostname: opt.hostname,
                    username: opt.username,
                    password: Password(opt.password),
                })
            }

            // Deserialize a map into SessionType directly
            fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                Deserialize::deserialize(MapAccessDeserializer::new(map))
            }
        }

        deserializer.deserialize_any(SessionTypeVisitor).map(Self)
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Sessions(pub HashMap<String, SessionTypeCompat>);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub log_level: LogLevel,
    #[serde(default)]
    pub sessions: Sessions,
    /// Curves that a source or a zone names with `curve`
    #[serde(default)]
    pub curves: HashMap<String, CurveConfig>,
    pub zones: Vec<Zone>,
}

/// A curve defined once and named by any number of sources and zones
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurveConfig {
    pub steps: Vec<Step>,
    /// Degrees the curve's temperature must fall before its duty follows
    #[serde(default)]
    pub hysteresis: u8,
}

/// Whether `value` is exactly `len` hex digits, in either case
fn is_hex_digits(value: &str, len: usize) -> bool {
    value.len() == len && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// Whether `gpu` is a full GPU UUID as nvidia-smi prints it:
/// `GPU-` and hex groups of 8, 4, 4, 4 and 12 digits.
fn is_gpu_uuid(gpu: &str) -> bool {
    const GROUP_LENGTHS: [usize; 5] = [8, 4, 4, 4, 12];

    let Some(uuid) = gpu.strip_prefix("GPU-") else {
        return false;
    };
    let groups: Vec<&str> = uuid.split('-').collect();

    groups.len() == GROUP_LENGTHS.len()
        && groups.iter().zip(GROUP_LENGTHS).all(|(group, len)| is_hex_digits(group, len))
}

/// Whether `device` is a PCI device number: two hex digits from 00 to 1f
fn is_pci_device(device: &str) -> bool {
    // The device number has five bits, so the first hex digit is 0 or 1
    is_hex_digits(device, 2) && matches!(device.as_bytes()[0], b'0' | b'1')
}

/// Whether `gpu` is a full PCI bus id, `DDDD:BB:DD.F` or `DDDDDDDD:BB:DD.F`,
/// with hex domain and bus, a device from 00 to 1f and a function from 0 to 7.
fn is_pci_bus_id(gpu: &str) -> bool {
    let parts: Vec<&str> = gpu.split(':').collect();
    let [domain, bus, device_function] = parts[..] else {
        return false;
    };
    let Some((device, function)) = device_function.split_once('.') else {
        return false;
    };

    (is_hex_digits(domain, 4) || is_hex_digits(domain, 8))
        && is_hex_digits(bus, 2)
        && is_pci_device(device)
        && matches!(function, "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7")
}

pub fn load_config(path: &Path) -> Result<Config> {
    let contents = fs::read_to_string(path)
        .map_err(|e| Error::Io { path: path.to_owned(), source: e })?;

    parse_config(&contents, path)
}

/// Parse and validate the contents of a config file. The path is only used in
/// error messages.
pub fn parse_config(contents: &str, path: &Path) -> Result<Config> {
    let mut config: Config = toml::from_str(contents)
        .map_err(|e| Error::ConfigParse { path: path.to_owned(), source: e })?;

    // Validate config

    // Create default session
    config.sessions.0.entry(SessionName::default().0)
        .or_default();

    check_sessions(&config.sessions)
        .map_err(|reason| Error::ConfigValidation { path: path.to_owned(), reason })?;

    if config.zones.is_empty() {
        return Err(Error::ConfigValidation {
            path: path.to_owned(),
            reason: "zones: must be non-empty".to_owned(),
        });
    }

    // Before the zone checks, so that a named curve meets every check of
    // inline steps
    resolve_curves(&mut config)
        .map_err(|reason| Error::ConfigValidation { path: path.to_owned(), reason })?;

    for (i, zone_config) in config.zones.iter().enumerate() {
        if zone_config.interval.0 == 0 {
            return Err(Error::ConfigValidation {
                path: path.to_owned(),
                reason: format!("zones[{}].interval: must be greater than 0", i),
            });
        }

        if zone_config.ipmi_zones.is_empty() {
            return Err(Error::ConfigValidation {
                path: path.to_owned(),
                reason: format!("zones[{}].ipmi_zones: must be non-empty", i),
            });
        } else if zone_config.sources.is_empty() {
            return Err(Error::ConfigValidation {
                path: path.to_owned(),
                reason: format!("zones[{}].sources: must be non-empty", i),
            });
        }

        for (j, source) in zone_config.sources.iter().enumerate() {
            if let OnFailure::Dcycle { dcycle } = source.on_failure {
                if dcycle > 100 {
                    return Err(Error::ConfigValidation {
                        path: path.to_owned(),
                        reason: format!("zones[{}].sources[{}].on_failure[type=dcycle].dcycle: invalid percentage: {}", i, j, dcycle),
                    });
                }
            }

            if source.smoothing_samples == 0 {
                return Err(Error::ConfigValidation {
                    path: path.to_owned(),
                    reason: format!("zones[{}].sources[{}].smoothing_samples: must be greater than 0", i, j),
                });
            }

            if let Source::Nvidia { gpu } = &source.kind {
                if !is_gpu_uuid(gpu) && !is_pci_bus_id(gpu) {
                    return Err(Error::ConfigValidation {
                        path: path.to_owned(),
                        reason: format!("zones[{}].sources[{}].gpu: select the card by PCI bus id or UUID: {:?}", i, j, gpu),
                    });
                }
            }
        }

        if !config.sessions.0.contains_key(&zone_config.session.0) {
            return Err(Error::ConfigValidation {
                path: path.to_owned(),
                reason: format!("zones[{}].session: {:?} does not exist", i, zone_config.session.0),
            });
        }

        if matches!(zone_config.aggregation, Aggregation::Average { top: Some(0) }) {
            return Err(Error::ConfigValidation {
                path: path.to_owned(),
                reason: format!("zones[{}].aggregation[type=average].top: must be greater than 0", i),
            });
        }

        if matches!(zone_config.duty_aggregation, Aggregation::Average { top: Some(0) }) {
            return Err(Error::ConfigValidation {
                path: path.to_owned(),
                reason: format!("zones[{}].duty_aggregation[type=average].top: must be greater than 0", i),
            });
        }

        if let Some(steps) = &zone_config.steps {
            check_steps(steps, &format!("zones[{}].steps", i))
                .map_err(|reason| Error::ConfigValidation { path: path.to_owned(), reason })?;
        }

        for (j, source) in zone_config.sources.iter().enumerate() {
            if let Some(steps) = &source.steps {
                // An empty curve gives 100%, which an average would dilute
                if steps.is_empty() {
                    let reason = match &source.curve {
                        Some(name) => format!("zones[{}].sources[{}].curve: curve {:?} has empty steps", i, j, name),
                        None => format!("zones[{}].sources[{}].steps: must be non-empty; remove the key to use the zone's steps", i, j),
                    };

                    return Err(Error::ConfigValidation { path: path.to_owned(), reason });
                }

                check_steps(steps, &format!("zones[{}].sources[{}].steps", i, j))
                    .map_err(|reason| Error::ConfigValidation { path: path.to_owned(), reason })?;
            }
        }

        let any_with_steps = zone_config.sources.iter().any(|s| s.steps.is_some());
        let any_without_steps = zone_config.sources.iter().any(|s| s.steps.is_none());

        if any_without_steps && zone_config.steps.is_none() {
            return Err(Error::ConfigValidation {
                path: path.to_owned(),
                reason: format!("zones[{}].steps: must be set when a source has no steps", i),
            });
        }

        let empty_zone_steps = matches!(&zone_config.steps, Some(steps) if steps.is_empty());

        if any_with_steps && any_without_steps && empty_zone_steps {
            let reason = match &zone_config.curve {
                Some(name) => format!("zones[{}].curve: curve {:?} has empty steps; the zone curve must be non-empty when a source has no steps", i, name),
                None => format!("zones[{}].steps: must be non-empty when a source has no steps", i),
            };

            return Err(Error::ConfigValidation { path: path.to_owned(), reason });
        }
    }

    Ok(config)
}

/// Check that no setting of a remote session holds a NUL byte, which a C string
/// cannot carry to freeipmi. The reason never shows the value, which may be a
/// password. Sessions are checked in name order, so the same config always
/// gives the same reason.
fn check_sessions(sessions: &Sessions) -> Result<(), String> {
    let mut names: Vec<&String> = sessions.0.keys().collect();
    names.sort();

    for name in names {
        let SessionType::Remote { hostname, username, password } = &sessions.0[name].0 else {
            continue;
        };
        let settings = [("hostname", hostname), ("username", username), ("password", &password.0)];

        for (key, value) in settings {
            if value.contains('\0') {
                return Err(format!("sessions.{}.{}: must not contain a NUL byte", name, key));
            }
        }
    }

    Ok(())
}

/// Check the `curves` table and every `curve` key, then copy each named curve
/// into the `steps` and `curve_hysteresis` of the source or zone that names
/// it, so that the runtime needs no `curves` table. Curves are checked in name
/// order, so the same config always gives the same reason.
fn resolve_curves(config: &mut Config) -> Result<(), String> {
    let mut names: Vec<&String> = config.curves.keys().collect();
    names.sort();

    for name in names {
        check_steps(&config.curves[name].steps, &format!("curves.{}.steps", name))?;
    }

    for (i, zone) in config.zones.iter_mut().enumerate() {
        if let Some(name) = &zone.curve {
            let curve = named_curve(&config.curves, name, zone.steps.is_some(), &format!("zones[{}].curve", i))?;

            zone.steps = Some(curve.steps.clone());
            zone.curve_hysteresis = Some(curve.hysteresis);
        }

        for (j, source) in zone.sources.iter_mut().enumerate() {
            if let Some(name) = &source.curve {
                let key = format!("zones[{}].sources[{}].curve", i, j);
                let curve = named_curve(&config.curves, name, source.steps.is_some(), &key)?;

                source.steps = Some(curve.steps.clone());
                source.curve_hysteresis = Some(curve.hysteresis);
            }
        }
    }

    Ok(())
}

/// The curve that `key` names, which must exist and must not stand beside
/// inline `steps`
fn named_curve<'a>(curves: &'a HashMap<String, CurveConfig>, name: &str, has_steps: bool, key: &str) -> Result<&'a CurveConfig, String> {
    if has_steps {
        return Err(format!("{}: set either steps or curve, not both", key));
    }

    curves.get(name).ok_or_else(|| format!("{}: unknown curve {:?}", key, name))
}

/// Check a curve's steps. `key` names the list in the reason, such as
/// `zones[0].steps`.
fn check_steps(steps: &[Step], key: &str) -> Result<(), String> {
    // The reason names the second step of the pair that breaks the order
    for (j, window) in steps.windows(2).enumerate() {
        if window[0].temp >= window[1].temp {
            return Err(format!("{}[{}].temp: values are not strictly increasing", key, j + 1));
        } else if window[0].dcycle > window[1].dcycle {
            return Err(format!("{}[{}].dcycle: values are not increasing", key, j + 1));
        }
    }

    for (j, &step) in steps.iter().enumerate() {
        if step.dcycle > 100 {
            return Err(format!("{}[{}].dcycle: invalid percentage: {}", key, j, step.dcycle));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        std::path::Path,
        super::{Aggregation, Config, LogLevel, OnFailure, parse_config, SessionType, Source},
        crate::{
            curve::{SourceReading, ZoneState},
            error::{Error, Result},
        },
    };

    const CONFIG_PATH: &str = "/etc/ipmi-fan-control.toml";

    /// Build a TOML config with one zone. Each override replaces the default
    /// value of its key or adds the key.
    fn zone_toml(overrides: &[(&str, &str)]) -> String {
        let mut fields = vec![
            ("ipmi_zones", "[0]"),
            ("sources", r#"[{ type = "ipmi", sensor = "CPU Temp" }]"#),
            ("steps", "[{ temp = 40, dcycle = 40 }, { temp = 70, dcycle = 70 }]"),
        ];

        for &(key, value) in overrides {
            match fields.iter_mut().find(|(k, _)| *k == key) {
                Some(field) => field.1 = value,
                None => fields.push((key, value)),
            }
        }

        let mut toml = "[[zones]]\n".to_owned();

        for (key, value) in fields {
            toml.push_str(&format!("{} = {}\n", key, value));
        }

        toml
    }

    fn parse(contents: &str) -> Result<Config> {
        parse_config(contents, Path::new(CONFIG_PATH))
    }

    fn validation_reason(contents: &str) -> String {
        match parse(contents) {
            Err(Error::ConfigValidation { path, reason }) => {
                assert_eq!(path, Path::new(CONFIG_PATH));
                reason
            }
            other => panic!("expected a validation error, got {:?}", other),
        }
    }

    #[test]
    fn sample_config_loads() {
        let config = parse(include_str!("../config.sample.toml")).unwrap();

        assert_eq!(config.zones.len(), 1);
        assert_eq!(config.zones[0].ipmi_zones, [0, 1]);
        assert!(!config.curves.is_empty());
    }

    #[test]
    fn sample_source_with_named_curve_loads_when_uncommented() {
        let sample = include_str!("../config.sample.toml");
        let line = sample.lines()
            .find(|l| l.trim_start().starts_with("# {") && l.contains("curve = "))
            .expect("the sample has a commented source with curve");
        let uncommented = line.replacen("# ", "", 1);
        let config = parse(&sample.replacen(line, &uncommented, 1)).unwrap();
        let source = config.zones[0].sources.last().unwrap();
        let name = source.curve.as_ref().unwrap();

        assert_eq!(source.steps.as_ref().unwrap().len(), config.curves[name].steps.len());
        assert_eq!(source.curve_hysteresis, Some(config.curves[name].hysteresis));
    }

    #[test]
    fn sample_zone_with_named_curve_loads_when_uncommented() {
        const ZONE_STEPS: &str = "steps = [\n  { temp = 40, dcycle = 40 },\n  { temp = 70, dcycle = 70 }\n]\n";
        let sample = include_str!("../config.sample.toml");
        let line = sample.lines()
            .find(|l| l.starts_with("#curve = "))
            .expect("the sample has a commented zone curve");
        assert!(sample.contains(ZONE_STEPS), "the sample has the zone steps");
        // The zone names the curve in place of its own steps
        let edited = sample.replacen(ZONE_STEPS, "", 1).replacen(line, &line[1..], 1);
        let config = parse(&edited).unwrap();
        let zone = &config.zones[0];
        let name = zone.curve.as_ref().unwrap();

        assert_eq!(zone.steps.as_ref().unwrap().len(), config.curves[name].steps.len());
        assert_eq!(zone.curve_hysteresis, Some(config.curves[name].hysteresis));
    }

    #[test]
    fn sample_source_with_steps_loads_when_uncommented() {
        let sample = include_str!("../config.sample.toml");
        let line = sample.lines()
            .find(|l| l.contains(r#"type = "nvidia""#) && l.contains("steps = "))
            .expect("the sample has a commented nvidia source with steps");
        let uncommented = line.replacen("# ", "", 1);
        let config = parse(&sample.replacen(line, &uncommented, 1)).unwrap();
        let sources = &config.zones[0].sources;

        assert!(matches!(sources.last().unwrap().kind, Source::Nvidia { .. }));
        assert_eq!(sources.last().unwrap().steps.as_ref().unwrap().len(), 2);
        assert_eq!(sources.last().unwrap().on_failure, OnFailure::Dcycle { dcycle: 80 });
    }

    #[test]
    fn sample_smoothed_source_loads_when_uncommented() {
        let sample = include_str!("../config.sample.toml");
        let line = sample.lines()
            .find(|l| l.trim_start().starts_with("# {") && l.contains("smoothing_samples = "))
            .expect("the sample has a commented source with smoothing_samples");
        let uncommented = line.replacen("# ", "", 1);
        let config = parse(&sample.replacen(line, &uncommented, 1)).unwrap();
        let sources = &config.zones[0].sources;

        assert_eq!(sources.last().unwrap().smoothing_samples, 3);
        assert!(sources[..sources.len() - 1].iter().all(|s| s.smoothing_samples == 1));
    }

    /// Every TOML block of the README
    fn readme_toml_blocks() -> Vec<&'static str> {
        const FENCE: &str = "```toml\n";

        include_str!("../README.md")
            .split(FENCE)
            .skip(1)
            .filter_map(|rest| rest.split_once("```").map(|(block, _)| block))
            .collect()
    }

    #[test]
    fn readme_on_failure_example_loads() {
        let blocks = readme_toml_blocks();
        let block = blocks.iter()
            .find(|b| b.contains("on_failure"))
            .expect("the README has a TOML example with on_failure");
        let config = parse(block).unwrap();
        let sources = &config.zones[0].sources;

        assert_eq!(sources[0].on_failure, OnFailure::Exit {});
        assert_eq!(sources[1].on_failure, OnFailure::Dcycle { dcycle: 80 });
    }

    #[test]
    fn readme_curve_example_loads() {
        let readme = include_str!("../README.md");
        let start = readme.find("```toml\n").expect("the README has a TOML example") + "```toml\n".len();
        let end = start + readme[start..].find("```").unwrap();
        let config = parse(&readme[start..end]).unwrap();
        let zone = &config.zones[0];

        assert!(zone.sources[0].steps.is_none());
        assert!(zone.sources[1].steps.is_some());
        assert!(matches!(zone.duty_aggregation, Aggregation::Maximum));
        assert_eq!(zone.hysteresis, 3);
    }

    #[test]
    fn readme_named_curve_example_loads() {
        let blocks = readme_toml_blocks();
        let block = blocks.iter()
            .find(|b| b.contains("[curves."))
            .expect("the README has a TOML example with a curves table");
        let config = parse(block).unwrap();
        let named: Vec<_> = config.zones[0].sources.iter().filter(|s| s.curve.is_some()).collect();

        assert!(named.len() >= 2, "{:?}", named);
        assert!(named.iter().all(|s| s.steps.is_some() && s.curve_hysteresis.is_some()));
    }

    #[test]
    fn minimal_config_uses_defaults() {
        let config = parse(&zone_toml(&[])).unwrap();

        assert!(matches!(config.log_level, LogLevel::Info));
        assert!(matches!(config.zones[0].aggregation, Aggregation::Maximum));
        assert_eq!(config.zones[0].session.0, "default");
        assert_eq!(config.sessions.0.len(), 1);
        assert!(matches!(config.sessions.0["default"].0, SessionType::Local));
    }

    #[test]
    fn ipmitool_session_arguments_parse() {
        let contents = format!("{}\n[sessions]\n{}\n",
            zone_toml(&[("session", r#""remote""#)]),
            r#"remote = ["-I", "lanplus", "-H", "bmc.lan", "-U", "admin", "-P", "secret"]"#);
        let config = parse(&contents).unwrap();

        match &config.sessions.0["remote"].0 {
            SessionType::Remote { hostname, username, password } => {
                assert_eq!(hostname, "bmc.lan");
                assert_eq!(username, "admin");
                assert_eq!(password.0, "secret");
            }
            other => panic!("expected a remote session, got {:?}", other),
        }
    }

    /// A config whose zone uses a remote session named `bmc` with the given
    /// hostname, username and password, each a TOML basic string body
    fn remote_session_toml(hostname: &str, username: &str, password: &str) -> String {
        format!("{}\n[sessions]\nbmc = {{ type = \"remote\", hostname = \"{}\", username = \"{}\", password = \"{}\" }}\n",
            zone_toml(&[("session", r#""bmc""#)]), hostname, username, password)
    }

    #[test]
    fn accepts_remote_session() {
        let config = parse(&remote_session_toml("bmc.lan", "admin", "secret")).unwrap();

        assert!(matches!(config.sessions.0["bmc"].0, SessionType::Remote { .. }));
    }

    #[test]
    fn rejects_nul_in_session_hostname() {
        assert_eq!(validation_reason(&remote_session_toml(r"bmc\u0000.lan", "admin", "secret")),
            "sessions.bmc.hostname: must not contain a NUL byte");
    }

    #[test]
    fn rejects_nul_in_session_username() {
        assert_eq!(validation_reason(&remote_session_toml("bmc.lan", r"ad\u0000min", "secret")),
            "sessions.bmc.username: must not contain a NUL byte");
    }

    #[test]
    fn rejects_nul_in_session_password_without_naming_it() {
        let reason = validation_reason(&remote_session_toml("bmc.lan", "admin", r"sec\u0000ret"));

        assert_eq!(reason, "sessions.bmc.password: must not contain a NUL byte");
    }

    #[test]
    fn rejects_nul_in_ipmitool_session_arguments() {
        let contents = format!("{}\n[sessions]\n{}\n",
            zone_toml(&[("session", r#""remote""#)]),
            r#"remote = ["-I", "lanplus", "-H", "bmc.lan", "-U", "admin", "-P", "sec\u0000ret"]"#);

        assert_eq!(validation_reason(&contents), "sessions.remote.password: must not contain a NUL byte");
    }

    #[test]
    fn nul_error_names_the_first_bad_session_in_name_order() {
        // Each parse builds a map with new hash keys, so an unsorted scan would
        // name `bravo` first in about half of the runs
        const RUNS: usize = 64;
        let contents = format!("{}\n[sessions]\n{}\n{}\n",
            zone_toml(&[("session", r#""alpha""#)]),
            r#"bravo = { type = "remote", hostname = "bmc.lan", username = "admin", password = "sec\u0000ret" }"#,
            r#"alpha = { type = "remote", hostname = "bmc.lan", username = "admin", password = "sec\u0000ret" }"#);

        for _ in 0..RUNS {
            assert_eq!(validation_reason(&contents), "sessions.alpha.password: must not contain a NUL byte");
        }
    }

    #[test]
    fn accepts_flat_segment_and_full_duty() {
        let steps = "[{ temp = 40, dcycle = 40 }, { temp = 60, dcycle = 40 }, { temp = 80, dcycle = 100 }]";
        let config = parse(&zone_toml(&[("steps", steps)])).unwrap();

        let zone_steps = config.zones[0].steps.as_ref().unwrap();
        assert_eq!(zone_steps.len(), 3);
        assert_eq!(zone_steps[2].dcycle, 100);
    }

    #[test]
    fn accepts_empty_steps() {
        let config = parse(&zone_toml(&[("steps", "[]")])).unwrap();

        assert!(config.zones[0].steps.as_ref().unwrap().is_empty());
    }

    // Pins today's result: `top` may exceed the number of sources
    #[test]
    fn accepts_average_top_above_source_count() {
        let aggregation = r#"{ type = "average", top = 3 }"#;
        let config = parse(&zone_toml(&[("aggregation", aggregation)])).unwrap();

        assert_eq!(config.zones[0].sources.len(), 1);
        assert!(matches!(config.zones[0].aggregation, Aggregation::Average { top: Some(3) }));
    }

    #[test]
    fn rejects_empty_zones() {
        assert_eq!(validation_reason("zones = []\n"), "zones: must be non-empty");
    }

    #[test]
    fn rejects_zero_interval() {
        assert_eq!(validation_reason(&zone_toml(&[("interval", "0")])),
            "zones[0].interval: must be greater than 0");
    }

    #[test]
    fn rejects_empty_ipmi_zones() {
        assert_eq!(validation_reason(&zone_toml(&[("ipmi_zones", "[]")])),
            "zones[0].ipmi_zones: must be non-empty");
    }

    #[test]
    fn rejects_empty_sources() {
        assert_eq!(validation_reason(&zone_toml(&[("sources", "[]")])),
            "zones[0].sources: must be non-empty");
    }

    #[test]
    fn rejects_unknown_session() {
        assert_eq!(validation_reason(&zone_toml(&[("session", r#""missing""#)])),
            r#"zones[0].session: "missing" does not exist"#);
    }

    #[test]
    fn rejects_average_top_zero() {
        let aggregation = r#"{ type = "average", top = 0 }"#;

        assert_eq!(validation_reason(&zone_toml(&[("aggregation", aggregation)])),
            "zones[0].aggregation[type=average].top: must be greater than 0");
    }

    #[test]
    fn rejects_non_increasing_temp() {
        let steps = "[{ temp = 40, dcycle = 40 }, { temp = 40, dcycle = 50 }]";

        assert_eq!(validation_reason(&zone_toml(&[("steps", steps)])),
            "zones[0].steps[1].temp: values are not strictly increasing");
    }

    #[test]
    fn rejects_decreasing_dcycle() {
        let steps = "[{ temp = 40, dcycle = 50 }, { temp = 70, dcycle = 40 }]";

        assert_eq!(validation_reason(&zone_toml(&[("steps", steps)])),
            "zones[0].steps[1].dcycle: values are not increasing");
    }

    #[test]
    fn rejects_dcycle_above_100() {
        let steps = "[{ temp = 40, dcycle = 40 }, { temp = 70, dcycle = 101 }]";

        assert_eq!(validation_reason(&zone_toml(&[("steps", steps)])),
            "zones[0].steps[1].dcycle: invalid percentage: 101");
    }

    #[test]
    fn reports_temp_error_before_dcycle_errors() {
        // Breaks the temp order, the dcycle order and the dcycle limit at once
        let steps = "[{ temp = 40, dcycle = 101 }, { temp = 40, dcycle = 40 }]";

        assert_eq!(validation_reason(&zone_toml(&[("steps", steps)])),
            "zones[0].steps[1].temp: values are not strictly increasing");
    }

    fn parse_error(contents: &str) -> String {
        match parse(contents) {
            Err(Error::ConfigParse { source, .. }) => source.to_string(),
            other => panic!("expected a parse error, got {:?}", other),
        }
    }

    #[test]
    fn rejects_unknown_key_in_source() {
        let sources = r#"[{ type = "file", path = "/sys/temp", scale = 1 }]"#;
        let message = parse_error(&zone_toml(&[("sources", sources)]));

        assert!(message.contains("unknown field `scale`"), "{}", message);
    }

    #[test]
    fn rejects_unknown_source_type() {
        let sources = r#"[{ type = "thermometer", path = "/sys/temp" }]"#;
        let message = parse_error(&zone_toml(&[("sources", sources)]));

        assert!(message.contains("unknown variant `thermometer`"), "{}", message);
    }

    /// Build a zone whose second source is an nvidia source with its own
    /// `steps`
    fn source_steps_toml(steps: &str) -> String {
        let sources = format!(r#"[{{ type = "ipmi", sensor = "CPU Temp" }}, {{ type = "nvidia", gpu = "{}", steps = {} }}]"#, GPU, steps);
        zone_toml(&[("sources", &sources)])
    }

    const GPU: &str = "00000000:81:00.0";
    const GPU_STEPS: &str = "[{ temp = 60, dcycle = 40 }, { temp = 90, dcycle = 100 }]";

    #[test]
    fn accepts_source_steps() {
        let config = parse(&source_steps_toml(GPU_STEPS)).unwrap();
        let sources = &config.zones[0].sources;

        assert!(sources[0].steps.is_none());
        assert!(matches!(&sources[1].kind, Source::Nvidia { gpu } if gpu == GPU));

        let steps = sources[1].steps.as_ref().unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!((steps[1].temp, steps[1].dcycle), (90, 100));
    }

    #[test]
    fn accepts_hysteresis() {
        let config = parse(&zone_toml(&[("hysteresis", "5")])).unwrap();

        assert_eq!(config.zones[0].hysteresis, 5);
    }

    #[test]
    fn hysteresis_defaults_to_zero() {
        let config = parse(&zone_toml(&[])).unwrap();

        assert_eq!(config.zones[0].hysteresis, 0);
    }

    #[test]
    fn accepts_average_duty_aggregation() {
        let aggregation = r#"{ type = "average", top = 2 }"#;
        let config = parse(&zone_toml(&[("duty_aggregation", aggregation)])).unwrap();

        assert!(matches!(config.zones[0].duty_aggregation, Aggregation::Average { top: Some(2) }));
    }

    #[test]
    fn duty_aggregation_defaults_to_maximum() {
        let config = parse(&zone_toml(&[])).unwrap();

        assert!(matches!(config.zones[0].duty_aggregation, Aggregation::Maximum));
    }

    #[test]
    fn rejects_source_steps_non_increasing_temp() {
        let steps = "[{ temp = 60, dcycle = 40 }, { temp = 60, dcycle = 50 }]";

        assert_eq!(validation_reason(&source_steps_toml(steps)),
            "zones[0].sources[1].steps[1].temp: values are not strictly increasing");
    }

    #[test]
    fn rejects_source_steps_decreasing_dcycle() {
        let steps = "[{ temp = 60, dcycle = 50 }, { temp = 90, dcycle = 40 }]";

        assert_eq!(validation_reason(&source_steps_toml(steps)),
            "zones[0].sources[1].steps[1].dcycle: values are not increasing");
    }

    #[test]
    fn rejects_empty_source_steps() {
        assert_eq!(validation_reason(&source_steps_toml("[]")),
            "zones[0].sources[1].steps: must be non-empty; remove the key to use the zone's steps");
    }

    #[test]
    fn rejects_empty_source_steps_when_every_source_has_steps() {
        let sources = format!(r#"[{{ type = "nvidia", gpu = "{}", steps = {} }}, {{ type = "file", path = "/sys/temp", steps = [] }}]"#, GPU, GPU_STEPS);

        assert_eq!(validation_reason(&zone_toml(&[("sources", &sources)])),
            "zones[0].sources[1].steps: must be non-empty; remove the key to use the zone's steps");
    }

    #[test]
    fn names_second_step_of_failing_temp_pair() {
        let steps = "[{ temp = 40, dcycle = 40 }, { temp = 50, dcycle = 50 }, { temp = 50, dcycle = 60 }]";

        assert_eq!(validation_reason(&zone_toml(&[("steps", steps)])),
            "zones[0].steps[2].temp: values are not strictly increasing");
    }

    #[test]
    fn names_second_step_of_failing_dcycle_pair() {
        let steps = "[{ temp = 60, dcycle = 40 }, { temp = 70, dcycle = 50 }, { temp = 90, dcycle = 45 }]";

        assert_eq!(validation_reason(&source_steps_toml(steps)),
            "zones[0].sources[1].steps[2].dcycle: values are not increasing");
    }

    #[test]
    fn rejects_source_steps_dcycle_above_100() {
        let steps = "[{ temp = 60, dcycle = 40 }, { temp = 90, dcycle = 101 }]";

        assert_eq!(validation_reason(&source_steps_toml(steps)),
            "zones[0].sources[1].steps[1].dcycle: invalid percentage: 101");
    }

    #[test]
    fn reports_zone_step_error_before_source_step_error() {
        let source_steps = "[{ temp = 60, dcycle = 40 }, { temp = 60, dcycle = 50 }]";
        let sources = format!(r#"[{{ type = "nvidia", gpu = "{}", steps = {} }}, {{ type = "ipmi", sensor = "CPU Temp" }}]"#, GPU, source_steps);
        let zone_steps = "[{ temp = 40, dcycle = 50 }, { temp = 70, dcycle = 40 }]";

        assert_eq!(validation_reason(&zone_toml(&[("sources", &sources), ("steps", zone_steps)])),
            "zones[0].steps[1].dcycle: values are not increasing");
    }

    #[test]
    fn rejects_unknown_key_beside_source_steps() {
        let sources = format!(r#"[{{ type = "file", path = "/sys/temp", steps = {}, scale = 1 }}]"#, GPU_STEPS);
        let message = parse_error(&zone_toml(&[("sources", &sources)]));

        assert!(message.contains("unknown field `scale`"), "{}", message);
    }

    #[test]
    fn rejects_unknown_source_type_with_steps() {
        let sources = format!(r#"[{{ type = "thermometer", path = "/sys/temp", steps = {} }}]"#, GPU_STEPS);
        let message = parse_error(&zone_toml(&[("sources", &sources)]));

        assert!(message.contains("unknown variant `thermometer`"), "{}", message);
    }

    #[test]
    fn rejects_repeated_key_in_source() {
        let sources = r#"[{ type = "file", path = "one", path = "two" }]"#;
        let message = parse_error(&zone_toml(&[("sources", sources)]));

        assert!(message.contains("duplicate"), "{}", message);
        assert!(message.contains("path"), "{}", message);
    }

    #[test]
    fn rejects_repeated_steps_in_source() {
        let sources = format!(r#"[{{ type = "file", path = "/sys/temp", steps = {}, steps = {} }}]"#, GPU_STEPS, GPU_STEPS);
        let message = parse_error(&zone_toml(&[("sources", &sources)]));

        assert!(message.contains("duplicate"), "{}", message);
        assert!(message.contains("steps"), "{}", message);
    }

    #[test]
    fn accepts_positional_sources() {
        let sources = r#"[["file", "/dev/null"], ["ipmi", "CPU Temp"]]"#;
        let config = parse(&zone_toml(&[("sources", sources)])).unwrap();
        let sources = &config.zones[0].sources;

        assert!(matches!(&sources[0].kind, Source::File { path } if path == "/dev/null"));
        assert!(matches!(&sources[1].kind, Source::Ipmi { sensor } if sensor == "CPU Temp"));
        assert!(sources.iter().all(|s| s.steps.is_none()));
    }

    #[test]
    fn rejects_positional_source_of_unknown_type() {
        let message = parse_error(&zone_toml(&[("sources", r#"[["thermometer", "/sys/temp"]]"#)]));

        assert!(message.contains("unknown variant `thermometer`"), "{}", message);
    }

    #[test]
    fn rejects_source_steps_of_wrong_type() {
        let message = parse_error(&source_steps_toml("5"));

        assert!(message.contains("steps: invalid type"), "{}", message);
    }

    /// Build a zone whose only source is a file source with `on_failure`
    fn on_failure_toml(on_failure: &str) -> String {
        let sources = format!(r#"[{{ type = "file", path = "/sys/temp", on_failure = {} }}]"#, on_failure);
        zone_toml(&[("sources", &sources)])
    }

    fn loaded_on_failure(on_failure: &str) -> OnFailure {
        let config = parse(&on_failure_toml(on_failure)).unwrap();

        config.zones[0].sources[0].on_failure.clone()
    }

    #[test]
    fn accepts_on_failure_exit() {
        assert_eq!(loaded_on_failure(r#"{ type = "exit" }"#), OnFailure::Exit {});
    }

    #[test]
    fn accepts_on_failure_ignore() {
        assert_eq!(loaded_on_failure(r#"{ type = "ignore" }"#), OnFailure::Ignore {});
    }

    #[test]
    fn accepts_on_failure_dcycle() {
        assert_eq!(loaded_on_failure(r#"{ type = "dcycle", dcycle = 80 }"#), OnFailure::Dcycle { dcycle: 80 });
    }

    #[test]
    fn accepts_on_failure_dcycle_of_full_duty() {
        assert_eq!(loaded_on_failure(r#"{ type = "dcycle", dcycle = 100 }"#), OnFailure::Dcycle { dcycle: 100 });
    }

    #[test]
    fn on_failure_defaults_to_exit() {
        let config = parse(&zone_toml(&[])).unwrap();

        assert_eq!(config.zones[0].sources[0].on_failure, OnFailure::Exit {});
    }

    #[test]
    fn positional_source_on_failure_defaults_to_exit() {
        let config = parse(&zone_toml(&[("sources", r#"[["file", "/dev/null"]]"#)])).unwrap();

        assert_eq!(config.zones[0].sources[0].on_failure, OnFailure::Exit {});
    }

    #[test]
    fn accepts_on_failure_beside_source_steps() {
        let sources = format!(r#"[{{ type = "nvidia", gpu = "{}", steps = {}, on_failure = {{ type = "ignore" }} }}]"#, GPU, GPU_STEPS);
        let config = parse(&zone_toml(&[("sources", &sources)])).unwrap();
        let source = &config.zones[0].sources[0];

        assert!(matches!(&source.kind, Source::Nvidia { gpu } if gpu == GPU));
        assert_eq!(source.steps.as_ref().unwrap().len(), 2);
        assert_eq!(source.on_failure, OnFailure::Ignore {});
    }

    #[test]
    fn rejects_on_failure_dcycle_above_100() {
        assert_eq!(validation_reason(&on_failure_toml(r#"{ type = "dcycle", dcycle = 101 }"#)),
            "zones[0].sources[0].on_failure[type=dcycle].dcycle: invalid percentage: 101");
    }

    /// Sources whose last entry has an `on_failure` dcycle of 101, after
    /// `good` sources without one
    fn bad_dcycle_last_of(good: usize) -> String {
        let mut sources = vec![r#"{ type = "file", path = "/sys/temp" }"#; good];
        sources.push(r#"{ type = "file", path = "/sys/temp", on_failure = { type = "dcycle", dcycle = 101 } }"#);

        format!("[{}]", sources.join(", "))
    }

    #[test]
    fn on_failure_dcycle_error_names_the_source_index() {
        let contents = zone_toml(&[("sources", &bad_dcycle_last_of(1))]);

        assert_eq!(validation_reason(&contents),
            "zones[0].sources[1].on_failure[type=dcycle].dcycle: invalid percentage: 101");
    }

    #[test]
    fn on_failure_dcycle_error_names_the_zone_index() {
        let contents = zone_toml(&[]) + &zone_toml(&[("sources", &bad_dcycle_last_of(2))]);

        assert_eq!(validation_reason(&contents),
            "zones[1].sources[2].on_failure[type=dcycle].dcycle: invalid percentage: 101");
    }

    #[test]
    fn rejects_unknown_on_failure_type() {
        let message = parse_error(&on_failure_toml(r#"{ type = "slow" }"#));

        assert!(message.contains("on_failure: unknown variant `slow`"), "{}", message);
    }

    #[test]
    fn rejects_on_failure_dcycle_without_dcycle() {
        let message = parse_error(&on_failure_toml(r#"{ type = "dcycle" }"#));

        assert!(message.contains("on_failure: missing field `dcycle`"), "{}", message);
    }

    #[test]
    fn rejects_extra_key_on_each_on_failure_type() {
        for on_failure in [
            r#"{ type = "exit", dcycle = 80 }"#,
            r#"{ type = "ignore", dcycle = 80 }"#,
            r#"{ type = "dcycle", dcycle = 80, floor = 1 }"#,
        ] {
            let message = parse_error(&on_failure_toml(on_failure));

            assert!(message.contains("on_failure: unknown field"), "{}: {}", on_failure, message);
        }
    }

    #[test]
    fn rejects_repeated_on_failure_in_source() {
        let sources = r#"[{ type = "file", path = "/sys/temp", on_failure = { type = "exit" }, on_failure = { type = "ignore" } }]"#;
        let message = parse_error(&zone_toml(&[("sources", sources)]));

        assert!(message.contains("duplicate field `on_failure`"), "{}", message);
    }

    /// Sources whose entry at `index` sets `smoothing_samples` to `value`,
    /// after plain sources
    fn smoothing_sources(index: usize, value: &str) -> String {
        let mut sources = vec![r#"{ type = "file", path = "/sys/temp" }"#.to_owned(); index];
        sources.push(format!(r#"{{ type = "file", path = "/sys/temp", smoothing_samples = {} }}"#, value));

        format!("[{}]", sources.join(", "))
    }

    #[test]
    fn accepts_smoothing_samples() {
        let config = parse(&zone_toml(&[("sources", &smoothing_sources(0, "3"))])).unwrap();

        assert_eq!(config.zones[0].sources[0].smoothing_samples, 3);
    }

    #[test]
    fn accepts_smoothing_samples_beside_steps_and_on_failure() {
        let sources = format!(r#"[{{ type = "nvidia", gpu = "{}", smoothing_samples = 4, steps = {}, on_failure = {{ type = "ignore" }} }}]"#, GPU, GPU_STEPS);
        let config = parse(&zone_toml(&[("sources", &sources)])).unwrap();
        let source = &config.zones[0].sources[0];

        assert!(matches!(&source.kind, Source::Nvidia { gpu } if gpu == GPU));
        assert_eq!(source.smoothing_samples, 4);
        assert_eq!(source.steps.as_ref().unwrap().len(), 2);
        assert_eq!(source.on_failure, OnFailure::Ignore {});
    }

    #[test]
    fn smoothing_samples_defaults_to_one() {
        let config = parse(&zone_toml(&[])).unwrap();

        assert_eq!(config.zones[0].sources[0].smoothing_samples, 1);
    }

    #[test]
    fn positional_source_smoothing_samples_defaults_to_one() {
        let config = parse(&zone_toml(&[("sources", r#"[["file", "/dev/null"]]"#)])).unwrap();

        assert_eq!(config.zones[0].sources[0].smoothing_samples, 1);
    }

    #[test]
    fn rejects_smoothing_samples_zero() {
        assert_eq!(validation_reason(&zone_toml(&[("sources", &smoothing_sources(0, "0"))])),
            "zones[0].sources[0].smoothing_samples: must be greater than 0");
    }

    #[test]
    fn smoothing_samples_zero_error_names_the_source_index() {
        assert_eq!(validation_reason(&zone_toml(&[("sources", &smoothing_sources(1, "0"))])),
            "zones[0].sources[1].smoothing_samples: must be greater than 0");
    }

    #[test]
    fn smoothing_samples_zero_error_names_the_zone_index() {
        let contents = zone_toml(&[]) + &zone_toml(&[("sources", &smoothing_sources(2, "0"))]);

        assert_eq!(validation_reason(&contents),
            "zones[1].sources[2].smoothing_samples: must be greater than 0");
    }

    #[test]
    fn rejects_smoothing_samples_above_255() {
        let message = parse_error(&zone_toml(&[("sources", &smoothing_sources(0, "256"))]));

        assert!(message.contains("smoothing_samples: invalid value"), "{}", message);
    }

    #[test]
    fn rejects_repeated_smoothing_samples_in_source() {
        let sources = r#"[{ type = "file", path = "/sys/temp", smoothing_samples = 3, smoothing_samples = 1 }]"#;
        let message = parse_error(&zone_toml(&[("sources", sources)]));

        assert!(message.contains("duplicate field `smoothing_samples`"), "{}", message);
    }

    #[test]
    fn rejects_mixed_sources_without_zone_steps() {
        let sources = format!(r#"[{{ type = "ipmi", sensor = "CPU Temp" }}, {{ type = "nvidia", gpu = "{}", steps = {} }}]"#, GPU, GPU_STEPS);

        assert_eq!(validation_reason(&zone_toml(&[("sources", &sources), ("steps", "[]")])),
            "zones[0].steps: must be non-empty when a source has no steps");
    }

    /// Drop the line that sets `key` from a config built by `zone_toml`
    fn omit_key(contents: &str, key: &str) -> String {
        let prefix = format!("{} = ", key);

        let lines: Vec<&str> = contents.lines()
            .filter(|line| !line.starts_with(&prefix))
            .collect();

        lines.join("\n") + "\n"
    }

    #[test]
    fn accepts_omitted_zone_steps_when_every_source_has_steps() {
        let sources = format!(r#"[{{ type = "nvidia", gpu = "{}", steps = {} }}, {{ type = "file", path = "/sys/temp", steps = {} }}]"#, GPU, GPU_STEPS, GPU_STEPS);
        let contents = omit_key(&zone_toml(&[("sources", &sources)]), "steps");
        let config = parse(&contents).unwrap();

        assert!(config.zones[0].sources.iter().all(|s| s.steps.is_some()));
    }

    #[test]
    fn rejects_omitted_zone_steps_when_no_source_has_steps() {
        let contents = omit_key(&zone_toml(&[]), "steps");

        assert_eq!(validation_reason(&contents),
            "zones[0].steps: must be set when a source has no steps");
    }

    #[test]
    fn rejects_omitted_zone_steps_when_a_source_has_no_steps() {
        let contents = omit_key(&source_steps_toml(GPU_STEPS), "steps");

        assert_eq!(validation_reason(&contents),
            "zones[0].steps: must be set when a source has no steps");
    }

    #[test]
    fn accepts_empty_zone_steps_when_every_source_has_steps() {
        let sources = format!(r#"[{{ type = "nvidia", gpu = "{}", steps = {} }}]"#, GPU, GPU_STEPS);
        let config = parse(&zone_toml(&[("sources", &sources), ("steps", "[]")])).unwrap();

        assert!(config.zones[0].steps.as_ref().unwrap().is_empty());
    }

    #[test]
    fn rejects_duty_aggregation_top_zero() {
        let aggregation = r#"{ type = "average", top = 0 }"#;

        assert_eq!(validation_reason(&zone_toml(&[("duty_aggregation", aggregation)])),
            "zones[0].duty_aggregation[type=average].top: must be greater than 0");
    }

    /// `contents` followed by a `[curves.gpu]` table with `steps` and the
    /// extra lines in `rest`
    fn with_gpu_curve(contents: &str, steps: &str, rest: &str) -> String {
        format!("{}\n[curves.gpu]\nsteps = {}\n{}", contents, steps, rest)
    }

    /// A zone of two file sources, each with the extra keys in `keys`
    fn two_sources_toml(keys: &str) -> String {
        let sources = format!(r#"[{{ type = "file", path = "/sys/a", {} }}, {{ type = "file", path = "/sys/b", {} }}]"#, keys, keys);

        omit_key(&zone_toml(&[("sources", &sources)]), "steps")
    }

    #[test]
    fn accepts_sources_that_name_a_curve() {
        let contents = with_gpu_curve(&two_sources_toml(r#"curve = "gpu""#), GPU_STEPS, "hysteresis = 3\n");
        let config = parse(&contents).unwrap();

        assert_eq!(config.curves["gpu"].hysteresis, 3);

        for source in &config.zones[0].sources {
            assert_eq!(source.curve.as_deref(), Some("gpu"));
            let steps = source.steps.as_ref().unwrap();
            assert_eq!(steps.iter().map(|s| (s.temp, s.dcycle)).collect::<Vec<_>>(), [(60, 40), (90, 100)]);
            assert_eq!(source.curve_hysteresis, Some(3));
        }
    }

    #[test]
    fn accepts_zone_that_names_a_curve() {
        let zone_keys = [("curve", r#""gpu""#), ("hysteresis", "5")];
        let contents = with_gpu_curve(&omit_key(&zone_toml(&zone_keys), "steps"), GPU_STEPS, "hysteresis = 3\n");
        let config = parse(&contents).unwrap();
        let zone = &config.zones[0];

        assert_eq!(zone.steps.as_ref().unwrap().len(), 2);
        assert_eq!(zone.hysteresis, 5);
        assert_eq!(zone.curve_hysteresis, Some(3));
    }

    #[test]
    fn curve_hysteresis_defaults_to_zero() {
        let zone_keys = [("curve", r#""gpu""#), ("hysteresis", "5")];
        let contents = with_gpu_curve(&omit_key(&zone_toml(&zone_keys), "steps"), GPU_STEPS, "");
        let config = parse(&contents).unwrap();

        assert_eq!(config.zones[0].curve_hysteresis, Some(0));
    }

    #[test]
    fn inline_steps_keep_the_zone_hysteresis() {
        let config = parse(&source_steps_toml(GPU_STEPS)).unwrap();
        let zone = &config.zones[0];

        assert_eq!(zone.curve_hysteresis, None);
        assert!(zone.sources.iter().all(|s| s.curve.is_none() && s.curve_hysteresis.is_none()));
    }

    /// The duties of the first zone of `contents` when both of its sources
    /// read each of `temps` in turn
    fn zone_duties(contents: &str, temps: &[u8]) -> Vec<u8> {
        let config = parse(contents).unwrap();
        let zone = &config.zones[0];
        let mut state = ZoneState::new(zone);

        temps.iter()
            .map(|&t| state.next_duty(zone, &[SourceReading::Temp(t), SourceReading::Temp(t)]).duty)
            .collect()
    }

    #[test]
    fn named_curve_gives_the_duties_of_the_same_inline_steps() {
        let named = with_gpu_curve(&two_sources_toml(r#"curve = "gpu""#), GPU_STEPS, "hysteresis = 3\n");
        let inline = two_sources_toml(&format!("steps = {}", GPU_STEPS)) + "hysteresis = 3\n";
        // A fall of 2 from 72 C is held; a fall of 4 is followed
        let temps = [60, 72, 70, 68];

        assert_eq!(zone_duties(&named, &temps), [40, 64, 64, 56]);
        assert_eq!(zone_duties(&inline, &temps), [40, 64, 64, 56]);
    }

    #[test]
    fn rejects_unknown_source_curve() {
        let sources = r#"[{ type = "ipmi", sensor = "CPU Temp" }, { type = "file", path = "/sys/temp", curve = "x" }]"#;

        assert_eq!(validation_reason(&zone_toml(&[("sources", sources)])),
            r#"zones[0].sources[1].curve: unknown curve "x""#);
    }

    #[test]
    fn rejects_unknown_zone_curve() {
        let contents = omit_key(&zone_toml(&[("curve", r#""x""#)]), "steps");

        assert_eq!(validation_reason(&contents), r#"zones[0].curve: unknown curve "x""#);
    }

    #[test]
    fn rejects_source_with_steps_and_curve() {
        let sources = format!(r#"[{{ type = "file", path = "/sys/temp", steps = {}, curve = "gpu" }}]"#, GPU_STEPS);
        let contents = with_gpu_curve(&zone_toml(&[("sources", &sources)]), GPU_STEPS, "");

        assert_eq!(validation_reason(&contents), "zones[0].sources[0].curve: set either steps or curve, not both");
    }

    #[test]
    fn rejects_zone_with_steps_and_curve() {
        let contents = with_gpu_curve(&zone_toml(&[("curve", r#""gpu""#)]), GPU_STEPS, "");

        assert_eq!(validation_reason(&contents), "zones[0].curve: set either steps or curve, not both");
    }

    #[test]
    fn rejects_curve_dcycle_above_100() {
        let contents = with_gpu_curve(&zone_toml(&[]), "[{ temp = 60, dcycle = 40 }, { temp = 90, dcycle = 101 }]", "");

        assert_eq!(validation_reason(&contents), "curves.gpu.steps[1].dcycle: invalid percentage: 101");
    }

    #[test]
    fn rejects_curve_non_increasing_temp() {
        let contents = with_gpu_curve(&zone_toml(&[]), "[{ temp = 60, dcycle = 40 }, { temp = 60, dcycle = 50 }]", "");

        assert_eq!(validation_reason(&contents), "curves.gpu.steps[1].temp: values are not strictly increasing");
    }

    #[test]
    fn rejects_empty_named_curve_on_a_source() {
        let sources = r#"[{ type = "file", path = "/sys/temp", curve = "gpu" }]"#;
        let contents = with_gpu_curve(&zone_toml(&[("sources", sources)]), "[]", "");

        assert_eq!(validation_reason(&contents), r#"zones[0].sources[0].curve: curve "gpu" has empty steps"#);
    }

    #[test]
    fn rejects_empty_named_zone_curve_when_a_source_has_no_steps() {
        let sources = format!(r#"[{{ type = "ipmi", sensor = "CPU Temp" }}, {{ type = "file", path = "/sys/temp", steps = {} }}]"#, GPU_STEPS);
        let zone = omit_key(&zone_toml(&[("sources", &sources), ("curve", r#""gpu""#)]), "steps");
        let contents = with_gpu_curve(&zone, "[]", "");

        assert_eq!(validation_reason(&contents),
            r#"zones[0].curve: curve "gpu" has empty steps; the zone curve must be non-empty when a source has no steps"#);
    }

    #[test]
    fn rejects_zone_without_a_curve_when_a_source_has_none() {
        let sources = r#"[{ type = "ipmi", sensor = "CPU Temp" }, { type = "file", path = "/sys/temp", curve = "gpu" }]"#;
        let contents = with_gpu_curve(&omit_key(&zone_toml(&[("sources", sources)]), "steps"), GPU_STEPS, "");

        assert_eq!(validation_reason(&contents), "zones[0].steps: must be set when a source has no steps");
    }

    #[test]
    fn rejects_unknown_key_in_curve() {
        let message = parse_error(&with_gpu_curve(&zone_toml(&[]), GPU_STEPS, "slope = 1\n"));

        assert!(message.contains("unknown field `slope`"), "{}", message);
    }

    #[test]
    fn rejects_repeated_curve_in_source() {
        let sources = r#"[{ type = "file", path = "/sys/temp", curve = "gpu", curve = "gpu" }]"#;
        let message = parse_error(&with_gpu_curve(&zone_toml(&[("sources", sources)]), GPU_STEPS, ""));

        assert!(message.contains("duplicate field `curve`"), "{}", message);
    }

    #[test]
    fn accepts_positional_zone() {
        let contents = r#"zones = [["default", 1, 2, 0, [0], [{ type = "file", path = "/dev/null" }], { type = "maximum" }, [{ temp = 40, dcycle = 70 }], { type = "maximum" }, 0]]"#;
        let config = parse(contents).unwrap();
        let zone = &config.zones[0];

        assert!(matches!(zone.duty_aggregation, Aggregation::Maximum));
        assert_eq!(zone.hysteresis, 0);
        assert!(zone.curve.is_none());
    }

    #[test]
    fn positional_source_has_no_curve() {
        let config = parse(&zone_toml(&[("sources", r#"[["file", "/dev/null"]]"#)])).unwrap();

        assert!(config.zones[0].sources[0].curve.is_none());
    }

    /// Build a zone whose second source is an nvidia source with `gpu`
    fn nvidia_zone_toml(gpu: &str) -> String {
        // Debug quoting escapes `\n` and `"` the way a TOML basic string does
        let sources = format!(r#"[{{ type = "ipmi", sensor = "CPU Temp" }}, {{ type = "nvidia", gpu = {:?} }}]"#, gpu);
        zone_toml(&[("sources", &sources)])
    }

    fn assert_nvidia_gpu_loads(gpu: &str) {
        let config = parse(&nvidia_zone_toml(gpu)).unwrap();

        match &config.zones[0].sources[1].kind {
            Source::Nvidia { gpu: loaded } => assert_eq!(loaded, gpu),
            other => panic!("expected an nvidia source, got {:?}", other),
        }
    }

    fn assert_nvidia_gpu_rejected(gpu: &str) {
        assert_eq!(validation_reason(&nvidia_zone_toml(gpu)),
            format!("zones[0].sources[1].gpu: select the card by PCI bus id or UUID: {:?}", gpu));
    }

    #[test]
    fn accepts_nvidia_pci_bus_id() {
        assert_nvidia_gpu_loads("00000000:81:00.0");
    }

    #[test]
    fn accepts_nvidia_pci_bus_id_with_short_domain_and_mixed_case() {
        assert_nvidia_gpu_loads("0000:aF:1f.7");
    }

    #[test]
    fn accepts_nvidia_uuid() {
        assert_nvidia_gpu_loads("GPU-3c1a2b4d-5E6F-7a8b-9c0d-1e2f3a4b5c6d");
    }

    #[test]
    fn rejects_nvidia_index() {
        assert_nvidia_gpu_rejected("0");
    }

    #[test]
    fn rejects_nvidia_index_list() {
        assert_nvidia_gpu_rejected("0,1");
    }

    #[test]
    fn rejects_nvidia_empty_gpu() {
        assert_nvidia_gpu_rejected("");
    }

    #[test]
    fn rejects_nvidia_bare_uuid_prefix() {
        assert_nvidia_gpu_rejected("GPU-");
    }

    #[test]
    fn rejects_nvidia_bare_colon() {
        assert_nvidia_gpu_rejected(":");
    }

    #[test]
    fn rejects_nvidia_two_bus_ids() {
        assert_nvidia_gpu_rejected("00000000:81:00.0,00000000:82:00.0");
    }

    #[test]
    fn rejects_nvidia_malformed_bus_ids() {
        for gpu in [
            " 00000000:81:00.0",
            "00000000:81:00.0\n",
            "000:81:00.0",
            "000000:81:00.0",
            "00000000:181:00.0",
            "00000000:81:0.0",
            "00000000:81:00.8",
            "00000000:81:00",
            "0000000g:81:00.0",
            "00000000:81:00:0",
        ] {
            assert_nvidia_gpu_rejected(gpu);
        }
    }

    #[test]
    fn accepts_nvidia_highest_pci_device() {
        assert_nvidia_gpu_loads("0000:00:1f.0");
    }

    #[test]
    fn rejects_nvidia_pci_device_above_1f() {
        assert_nvidia_gpu_rejected("0000:00:20.0");
        assert_nvidia_gpu_rejected("00000000:81:ff.0");
    }

    #[test]
    fn rejects_nvidia_non_hex_bus() {
        assert_nvidia_gpu_rejected("00000000:8g:00.0");
    }

    #[test]
    fn rejects_nvidia_non_hex_device() {
        assert_nvidia_gpu_rejected("00000000:81:0z.0");
    }

    #[test]
    fn rejects_nvidia_malformed_uuids() {
        for gpu in [
            "GPU-3c1a2b4d-5e6f-7a8b-9c0d-1e2f3a4b5c6",
            "GPU-3c1a2b4d-5e6f-7a8b-9c0d-1e2f3a4b5c6d7",
            "GPU-3c1a2b4d5e6f-7a8b-9c0d-1e2f3a4b5c6d",
            "GPU-3c1a2b4d-5e6f-7a8b-9c0d-1e2f3a4b5c6g",
            "gpu-3c1a2b4d-5e6f-7a8b-9c0d-1e2f3a4b5c6d",
            "3c1a2b4d-5e6f-7a8b-9c0d-1e2f3a4b5c6d",
            "GPU-3c1a2b4d-5e6f-7a8b-9c0d-1e2f3a4b5c6d-",
        ] {
            assert_nvidia_gpu_rejected(gpu);
        }
    }
}
