mod bindings;
mod command;
mod config;
mod curve;
mod error;
mod fan;
mod freeipmi;
mod hwmon;
mod nvml;
mod source;
mod ipmi;
mod session;

use {
    std::{
        collections::HashMap,
        env,
        io,
        path::PathBuf,
        process,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    },
    clap::Parser,
    log::{debug, error, info, trace},
    tokio::{
        task::{self, JoinSet},
        time::sleep,
    },

    config::{Config, FanConfig, load_config, Zone},
    curve::{Transition, ZoneState},
    error::{Error, Result},
    fan::{build_fan, FanGuard, ReleaseFailures, ReleaseOutcome},
    freeipmi::SensorReading,
    hwmon::HwmonRoot,
    nvml::{Nvml, RealNvml},
    session::IpmiSession,
    source::{read_zone_sources, ZoneReaders},
};

static LOGGING_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// The path that names a failure to listen for signals in its error
const SIGNALS_PATH: &str = "(interrupt)";

/// The signals that stop the program. From the moment it listens, a signal
/// no longer ends the process at once; it waits here until `recv`.
#[cfg(unix)]
struct Signals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Signals {
    fn listen() -> io::Result<Self> {
        use tokio::signal::unix::{signal, SignalKind};

        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    /// Wait for a signal, which may have arrived before the call
    async fn recv(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
    }
}

#[cfg(windows)]
struct Signals {
    ctrl_break: tokio::signal::windows::CtrlBreak,
    ctrl_c: tokio::signal::windows::CtrlC,
}

#[cfg(windows)]
impl Signals {
    fn listen() -> io::Result<Self> {
        use tokio::signal::windows::{ctrl_break, ctrl_c};

        Ok(Self { ctrl_break: ctrl_break()?, ctrl_c: ctrl_c()? })
    }

    /// Wait for a signal, which may have arrived before the call
    async fn recv(&mut self) {
        tokio::select! {
            _ = self.ctrl_break.recv() => {}
            _ = self.ctrl_c.recv() => {}
        }
    }
}

/// Listen for signals, then open the app. A signal that arrives while the
/// app opens sessions waits until the zone loops run, so it stops the program
/// through the exit path that releases the fans and restores the fan mode.
fn start_up<S, A>(listen: impl FnOnce() -> io::Result<S>, open: impl FnOnce() -> Result<A>) -> Result<(S, A)> {
    let signals = listen().map_err(|source| Error::Io { path: SIGNALS_PATH.into(), source })?;
    let app = open()?;

    Ok((signals, app))
}

struct MainApp {
    config: Config,
    sessions: HashMap<String, Arc<IpmiSession>>,
    /// The one NVML of the process, which every zone shares. It loads the
    /// library only when a fan first calls it.
    nvml: Arc<dyn Nvml>,
    /// The failed releases of every zone's fans
    release_failures: ReleaseFailures,
}

impl MainApp {
    fn new(config: Config) -> Result<Self> {
        let mut sessions = HashMap::new();

        for (name, restore_zones) in sessions_to_open(&config) {
            let st = &config.sessions.0[&name].0;
            let session = IpmiSession::new(&name, st, restore_zones)?;

            sessions.insert(name, Arc::new(session));
        }

        Ok(Self {
            config,
            sessions,
            nvml: Arc::new(RealNvml::new()),
            release_failures: ReleaseFailures::default(),
        })
    }

    /// Run asynchronous loops for each zone. Returns when interrupted by one
    /// of `signals` (eg. ^C) or if a fatal error occurs.
    async fn run(&mut self, signals: &mut Signals) -> Result<()> {
        let mut loops = JoinSet::new();

        for zone_config in &self.config.zones {
            loops.spawn(Self::zone_loop(
                // Only a zone with a Supermicro fan has an open session
                self.sessions.get(&zone_config.session.0).cloned(),
                self.nvml.clone(),
                self.release_failures.clone(),
                // Cloned since there's no structured concurrency support yet
                Arc::new(zone_config.clone()),
            ));
        }

        let mut first_result = None;

        loop {
            let ret: Result<()> = tokio::select! {
                // Explicitly interrupted by ^C or signal handler
                _ = signals.recv() => {
                    info!("Interrupted");
                    Ok(())
                }
                // Oh boy, this is an Option<Result<Result<()>, JoinError>>
                r = loops.join_next() => {
                    match r {
                        // No tasks left
                        None => break,
                        // The task panicked or was aborted
                        Some(Err(e)) => {
                            if e.is_cancelled() {
                                Ok(())
                            } else {
                                Err(Error::LoopPanicked(e))
                            }
                        },
                        // zone_loop's actual error return value
                        Some(Ok(r)) => r,
                    }
                }
            };

            if first_result.is_none() {
                first_result = Some(ret);
            }

            // If tokio::select returned, then a loop exited or the program was
            // explicitly interrupted. Interrupt all remaining tasks and the
            // loop will exit once the FuturesUnordered is empty. This mechanism
            // is necessary because Tokio's JoinHandles do not cancel tasks when
            // they are dropped. Without the explicit aborts and joins, the
            // IpmiSession destructors might not run since the tasks would keep
            // the Arcs alive.
            loops.abort_all();
        }

        first_result.unwrap_or(Ok(()))
    }

    /// Main loop for a zone. The loop runs forever while the future is being
    /// polled.
    async fn zone_loop(
        session: Option<Arc<IpmiSession>>,
        nvml: Arc<dyn Nvml>,
        release_failures: ReleaseFailures,
        zone_config: Arc<Zone>,
    ) -> Result<()> {
        // Each Supermicro fan holds an Arc of the session, so this guard
        // releases the zone's fans before the session's own drop restores the
        // fan mode. A returned error, a panic and an abort of this task all
        // drop the guard; an abort takes effect once the poll in progress
        // returns.
        let mut fans = FanGuard::new(release_failures);
        let hwmon = HwmonRoot::default();

        for fan_config in &zone_config.fan_list {
            fans.add(build_fan(fan_config, session.as_ref(), &hwmon, &nvml)?)?;
        }

        info!("[{}] Starting loop for fans {:?}", zone_label(session.as_deref()), fans.descriptions());

        let mut state = ZoneState::new(&zone_config);
        let mut readers = ZoneReaders::new(hwmon);

        loop {
            let st = &mut state;
            let r = &mut readers;
            let f = &mut fans;

            task::block_in_place(|| {
                Self::update_duty_cycle(session.as_deref(), &zone_config, st, r, f)
            })?;

            sleep(zone_config.interval.to_duration()).await;
        }
    }

    /// Update fan PWM duty cycle from the zone's source readings and curves
    fn update_duty_cycle(
        session: Option<&IpmiSession>,
        zone_config: &Zone,
        state: &mut ZoneState,
        readers: &mut ZoneReaders,
        fans: &mut FanGuard,
    ) -> Result<()> {
        trace!("Querying sources for fans {:?}", fans.descriptions());

        let label = zone_label(session);
        let mut query_ipmi = || query_sensors(session);
        let failed_last = state.failed_sources();
        let readings = read_zone_sources(zone_config, &failed_last, &mut query_ipmi,
            &mut |index, source| readers.read(index, source))?;
        let update = state.next_duty(zone_config, &readings);

        Self::log_transitions(label, &fans.descriptions(), zone_config, &update.transitions);

        let applied = fans.set_duty(update.duty)?;

        for (fan, applied) in fans.descriptions().iter().zip(&applied) {
            debug!("[{}] {}: smoothed_temps={:?}C, curve_temps={:?}C, dcycle_cur={}, dcycle_new={}%, dcycle_sent={}",
                   label, fan, update.smoothed_temps, update.curve_temps,
                   percent(applied.before), update.duty, percent(applied.sent));
        }

        Ok(())
    }

    /// Log a source that starts failing as an error and one that reads again
    /// as information, once per change
    fn log_transitions(session_name: &str, fans: &[&str], zone_config: &Zone, transitions: &[Transition]) {
        let describe = |index: usize| {
            zone_config.sources.get(index).map_or_else(
                || format!("#{}", index),
                |s| format!("{:?} (on_failure {:?})", s.kind, s.on_failure))
        };

        for transition in transitions {
            match transition {
                Transition::Failed { source, error } => {
                    error!("[{}] Fans {:?}: source {} failed: {}",
                           session_name, fans, describe(*source), error);
                }
                Transition::Recovered { source } => {
                    info!("[{}] Fans {:?}: source {} reads again",
                          session_name, fans, describe(*source));
                }
            }
        }
    }
}

/// The result of the process: an error from the zone loops is kept, and a
/// clean run becomes an error when a fan failed to release
fn exit_result(run: Result<()>, releases: ReleaseOutcome) -> Result<()> {
    match (run, releases) {
        (Ok(()), ReleaseOutcome::SomeFailed) => Err(Error::FansNotReleased),
        (run, _) => run,
    }
}

/// Each session that a zone's Supermicro fan uses, in the order of
/// `config.sessions`, with the IPMI zones its drop sets to 100%
fn sessions_to_open(config: &Config) -> Vec<(String, Vec<u8>)> {
    let mut sessions = Vec::new();

    for name in config.sessions.0.keys() {
        let restore_zones: Vec<_> = config.zones
            .iter()
            .filter(|z| &z.session.0 == name)
            .flat_map(|z| &z.fan_list)
            .filter_map(|fan| match fan {
                FanConfig::Supermicro { zone } => Some(*zone),
                FanConfig::Hwmon { .. } | FanConfig::Nvidia { .. } => None,
            })
            .collect();

        // Don't waste resources if nothing would use the session
        if restore_zones.is_empty() {
            continue;
        }

        sessions.push((name.clone(), restore_zones));
    }

    sessions
}

/// The readings of every IPMI temperature sensor through `session`. A zone
/// without a session has no sensors to query.
fn query_sensors(session: Option<&IpmiSession>) -> Result<HashMap<String, Option<SensorReading>>> {
    match session {
        Some(session) => session.lock().get_temperature_readings(),
        None => Err(Error::SensorsWithoutSession),
    }
}

/// The tag of a zone's log lines when the zone has no IPMI session
const NO_SESSION_LABEL: &str = "no session";

/// The tag of a zone's log lines: its session's name, when it has one
fn zone_label(session: Option<&IpmiSession>) -> &str {
    session.map_or(NO_SESSION_LABEL, IpmiSession::name)
}

/// A duty cycle for the debug log, or `-` when there is none
fn percent(duty: Option<u8>) -> String {
    duty.map_or_else(|| "-".to_owned(), |d| format!("{}%", d))
}

fn bool_env(name: &str, default: bool) -> bool {
    let value = env::var(name);

    match value {
        Ok(v) if v == "true" || v == "t" || v == "yes" || v == "y" || v == "1" => true,
        Ok(v) if v == "false" || v == "f" || v == "no" || v == "n" || v == "0" => false,
        _ => default,
    }
}

#[derive(Debug, Parser)]
struct Opt {
    /// Path to config file
    #[clap(short, long)]
    config: PathBuf,
}

async fn main_wrapper() -> Result<()> {
    let opt = Opt::parse();
    let config = load_config(&opt.config)?;

    let pkg_name = env!("CARGO_PKG_NAME").replace('-', "_");

    // RUST_LOG has higher precedence than the config file option because it has
    // more flexibility (eg. turning on logs for dependencies)
    let mut builder = env_logger::Builder::from_env(
        env_logger::Env::default()
            .default_filter_or(format!("{}={}", pkg_name, config.log_level)));

    // Don't include timestamps in the log if requested (eg. if logs are going
    // to something like journald that already has timestamps)
    if !bool_env(&format!("{}_LOG_TIMESTAMPS", pkg_name.to_uppercase()), true) {
        builder.format_timestamp(None);
    }

    builder.init();
    LOGGING_INITIALIZED.store(true, Ordering::SeqCst);

    trace!("Loaded config: {:#?}", config);

    let (mut signals, mut app) = start_up(Signals::listen, || MainApp::new(config))?;
    let run = app.run(&mut signals).await;

    // Every zone task has ended, so every guard has released its fans
    exit_result(run, app.release_failures.outcome())
}

#[tokio::main]
async fn main() {
    match main_wrapper().await {
        Ok(_) => {}
        Err(e) => {
            if LOGGING_INITIALIZED.load(Ordering::SeqCst) {
                error!("{}", e);
            } else {
                eprintln!("{}", e);
            }
            process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        std::{
            cell::{Cell, RefCell},
            io,
            path::Path,
            process,
            time::Duration,
        },
        clap::CommandFactory,
        tokio::time::timeout,
        super::{exit_result, Opt, query_sensors, sessions_to_open, Signals, start_up},
        crate::{
            config::{Config, FanConfig, parse_config},
            error::Error,
            fan::ReleaseOutcome,
        },
    };

    fn parse(contents: &str) -> Config {
        parse_config(contents, Path::new("/etc/coolerd.toml")).unwrap()
    }

    /// `sessions_to_open` sorted by session name, because sessions are a map
    fn sorted_sessions(config: &Config) -> Vec<(String, Vec<u8>)> {
        let mut sessions = sessions_to_open(config);
        sessions.sort();
        sessions
    }

    #[test]
    fn command_name_is_coolerd() {
        assert_eq!(Opt::command().get_name(), "coolerd");
    }

    #[test]
    fn each_session_restores_only_its_own_zones() {
        let config = parse(concat!(
            "[[zones]]\n",
            "ipmi_zones = [0]\n",
            "sources = [{ type = \"ipmi\", sensor = \"CPU Temp\" }]\n",
            "steps = [{ temp = 40, dcycle = 40 }]\n",
            "[[zones]]\n",
            "session = \"bmc\"\n",
            "fans = [{ type = \"supermicro\", zone = 1 }]\n",
            "sources = [{ type = \"ipmi\", sensor = \"CPU Temp\" }]\n",
            "steps = [{ temp = 40, dcycle = 40 }]\n",
            "[sessions]\n",
            "bmc = { type = \"remote\", hostname = \"bmc.lan\", username = \"admin\", password = \"secret\" }\n",
        ));

        assert_eq!(sorted_sessions(&config), [("bmc".to_owned(), vec![1]), ("default".to_owned(), vec![0])]);
    }

    #[test]
    fn sample_config_opens_one_session() {
        let config = parse(include_str!("../config.sample.toml"));

        assert_eq!(sessions_to_open(&config), [("default".to_owned(), vec![0, 1])]);
    }

    #[test]
    fn hwmon_only_config_opens_no_session() {
        let config = parse(concat!(
            "[[zones]]\n",
            "fans = [{ type = \"hwmon\", chip = \"nct6798\", pwm = 2 }]\n",
            "sources = [{ type = \"hwmon\", chip = \"k10temp\", input = 1 }]\n",
            "steps = [{ temp = 40, dcycle = 40 }]\n",
            "[[zones]]\n",
            "session = \"bmc\"\n",
            "fans = [{ type = \"hwmon\", chip = \"nct6798\", pwm = 3 }]\n",
            "sources = [{ type = \"file\", path = \"/sys/temp\" }]\n",
            "steps = [{ temp = 40, dcycle = 40 }]\n",
            "[sessions]\n",
            "bmc = { type = \"remote\", hostname = \"bmc.lan\", username = \"admin\", password = \"secret\" }\n",
        ));

        assert_eq!(sessions_to_open(&config), []);
    }

    #[test]
    fn readme_hwmon_zone_opens_no_session() {
        let readme = include_str!("../README.md");
        let block = readme.split("```toml\n")
            .skip(1)
            .filter_map(|rest| rest.split_once("```").map(|(block, _)| block))
            .find(|b| b.contains("type = \"hwmon\"") && !b.contains("supermicro"))
            .expect("the README has a TOML example of a hwmon zone");
        let config = parse(block);

        assert_eq!(config.zones[0].fan_list, [FanConfig::Hwmon { chip: "nct6798".into(), pwm: 2 }]);
        assert_eq!(sessions_to_open(&config), []);
    }

    #[test]
    fn session_restores_only_its_supermicro_zones() {
        let config = parse(concat!(
            "[[zones]]\n",
            "fans = [{ type = \"hwmon\", chip = \"nct6798\", pwm = 2 }, { type = \"supermicro\", zone = 1 }]\n",
            "sources = [{ type = \"ipmi\", sensor = \"CPU Temp\" }]\n",
            "steps = [{ temp = 40, dcycle = 40 }]\n",
        ));

        assert_eq!(sessions_to_open(&config), [("default".to_owned(), vec![1])]);
    }

    #[test]
    fn session_restores_no_nvidia_fan() {
        let config = parse(concat!(
            "[[zones]]\n",
            "fans = [{ type = \"nvidia\", gpu = \"00000000:01:00.0\", fan = 0 }, { type = \"supermicro\", zone = 1 }]\n",
            "sources = [{ type = \"ipmi\", sensor = \"CPU Temp\" }]\n",
            "steps = [{ temp = 40, dcycle = 40 }]\n",
            "[[zones]]\n",
            "session = \"bmc\"\n",
            "fans = [{ type = \"nvidia\", gpu = \"00000000:81:00.0\", fan = 0 }]\n",
            "sources = [{ type = \"nvidia\", gpu = \"00000000:81:00.0\" }]\n",
            "steps = [{ temp = 40, dcycle = 40 }]\n",
            "[sessions]\n",
            "bmc = { type = \"remote\", hostname = \"bmc.lan\", username = \"admin\", password = \"secret\" }\n",
        ));

        assert_eq!(sorted_sessions(&config), [("default".to_owned(), vec![1])]);
    }

    #[test]
    fn start_up_listens_for_signals_before_it_opens_the_app() {
        let steps = RefCell::new(Vec::new());

        let started = start_up(
            || { steps.borrow_mut().push("listen"); Ok("signals") },
            || { steps.borrow_mut().push("open"); Ok("app") },
        );

        assert_eq!(started.unwrap(), ("signals", "app"));
        assert_eq!(steps.into_inner(), ["listen", "open"]);
    }

    #[test]
    fn start_up_opens_nothing_when_it_cannot_listen() {
        let opened = Cell::new(false);

        let started = start_up(
            || Err::<(), _>(io::Error::other("no signal driver")),
            || { opened.set(true); Ok(()) },
        );

        assert!(matches!(&started, Err(Error::Io { path, .. }) if path == Path::new("(interrupt)")), "{:?}", started);
        assert!(!opened.get());
    }

    /// A signal sent to this process after it listens, but before anything
    /// waits for one, is delivered at the first wait. The process handles
    /// SIGINT from the moment it listens, so the signal does not stop it.
    #[cfg(unix)]
    #[tokio::test]
    async fn signal_before_the_first_wait_is_delivered() {
        let mut signals = Signals::listen().unwrap();

        let status = process::Command::new("kill")
            .args(["-INT", &process::id().to_string()])
            .status()
            .unwrap();
        let delivered = timeout(Duration::from_secs(2), signals.recv()).await;

        assert!(status.success(), "{:?}", status);
        assert!(delivered.is_ok(), "no signal within 2 seconds");
    }

    #[test]
    fn clean_run_with_a_failed_release_is_an_error() {
        let result = exit_result(Ok(()), ReleaseOutcome::SomeFailed);

        assert!(matches!(result, Err(Error::FansNotReleased)), "{:?}", result);
    }

    #[test]
    fn clean_run_with_every_fan_released_succeeds() {
        let result = exit_result(Ok(()), ReleaseOutcome::AllReleased);

        assert!(result.is_ok(), "{:?}", result);
    }

    #[test]
    fn run_error_is_kept_whatever_the_releases_did() {
        for releases in [ReleaseOutcome::SomeFailed, ReleaseOutcome::AllReleased] {
            let result = exit_result(Err(Error::SensorsWithoutSession), releases);

            assert!(matches!(result, Err(Error::SensorsWithoutSession)), "{:?}", result);
        }
    }

    #[test]
    fn sensor_query_without_a_session_fails() {
        let result = query_sensors(None);

        assert!(matches!(result, Err(Error::SensorsWithoutSession)), "{:?}", result);
    }
}
