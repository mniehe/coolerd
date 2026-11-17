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
    pub sources: Vec<Source>,
    #[serde(default)]
    pub aggregation: Aggregation,
    pub steps: Vec<Step>,
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
    pub zones: Vec<Zone>,
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

    if config.zones.is_empty() {
        return Err(Error::ConfigValidation {
            path: path.to_owned(),
            reason: "zones: must be non-empty".to_owned(),
        });
    }

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

        for window in zone_config.steps.windows(2) {
            if window[0].temp >= window[1].temp {
                return Err(Error::ConfigValidation {
                    path: path.to_owned(),
                    reason: format!("zones[{}].steps[*].temp: values are not strictly increasing", i),
                });
            } else if window[0].dcycle > window[1].dcycle {
                return Err(Error::ConfigValidation {
                    path: path.to_owned(),
                    reason: format!("zones[{}].steps[*].dcycle: values are not increasing", i),
                });
            }
        }

        for (j, &step) in zone_config.steps.iter().enumerate() {
            if step.dcycle > 100 {
                return Err(Error::ConfigValidation {
                    path: path.to_owned(),
                    reason: format!("zones[{}].steps[{}].dcycle: invalid percentage: {}", i, j, step.dcycle),
                });
            }
        }
    }

    Ok(config)
}

#[cfg(test)]
mod tests {
    use {
        std::path::Path,
        super::{Aggregation, Config, LogLevel, parse_config, SessionType},
        crate::error::{Error, Result},
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

    #[test]
    fn accepts_flat_segment_and_full_duty() {
        let steps = "[{ temp = 40, dcycle = 40 }, { temp = 60, dcycle = 40 }, { temp = 80, dcycle = 100 }]";
        let config = parse(&zone_toml(&[("steps", steps)])).unwrap();

        assert_eq!(config.zones[0].steps.len(), 3);
        assert_eq!(config.zones[0].steps[2].dcycle, 100);
    }

    #[test]
    fn accepts_empty_steps() {
        let config = parse(&zone_toml(&[("steps", "[]")])).unwrap();

        assert!(config.zones[0].steps.is_empty());
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
            "zones[0].steps[*].temp: values are not strictly increasing");
    }

    #[test]
    fn rejects_decreasing_dcycle() {
        let steps = "[{ temp = 40, dcycle = 50 }, { temp = 70, dcycle = 40 }]";

        assert_eq!(validation_reason(&zone_toml(&[("steps", steps)])),
            "zones[0].steps[*].dcycle: values are not increasing");
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
            "zones[0].steps[*].temp: values are not strictly increasing");
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
}
