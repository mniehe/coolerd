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
    session::{IpmiSession, RestoreFailures, RestoreOutcome},
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
    /// The failed fan-mode restores of every session
    restore_failures: RestoreFailures,
}

impl MainApp {
    fn new(config: Config) -> Result<Self> {
        let mut sessions = HashMap::new();
        let restore_failures = RestoreFailures::default();

        for (name, restore_zones) in sessions_to_open(&config) {
            let st = &config.sessions.0[&name].0;
            let session = IpmiSession::new(&name, st, restore_zones, restore_failures.clone())?;

            sessions.insert(name, Arc::new(session));
        }

        Ok(Self {
            config,
            sessions,
            nvml: Arc::new(RealNvml::new()),
            release_failures: ReleaseFailures::default(),
            restore_failures,
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

        let mut results = RunResults::default();

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

            if let Some(e) = results.add(ret) {
                error!("{}", e);
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

        results.into_result()
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

/// The results of the zone loops and of the signals, as `run` receives them.
/// The first error is the result of the run; a signal or a cancelled loop
/// adds none, so an error that a zone returns after a signal still counts.
#[derive(Debug, Default)]
struct RunResults {
    first_error: Option<Error>,
}

impl RunResults {
    /// Add one result. Returns an error that is not kept, for the caller to
    /// log.
    fn add(&mut self, result: Result<()>) -> Option<Error> {
        let Err(e) = result else {
            return None;
        };

        if self.first_error.is_some() {
            return Some(e);
        }

        self.first_error = Some(e);

        None
    }

    /// The result of the run
    fn into_result(self) -> Result<()> {
        self.first_error.map_or(Ok(()), Err)
    }
}

/// The result of the process: an error from the zone loops is kept, and a
/// clean run becomes an error when a fan failed to release or a session
/// failed to restore its fan mode
fn exit_result(run: Result<()>, releases: ReleaseOutcome, restores: RestoreOutcome) -> Result<()> {
    match (run, releases, restores) {
        (Err(e), _, _) => Err(e),
        (Ok(()), ReleaseOutcome::SomeFailed, _) => Err(Error::FansNotReleased),
        (Ok(()), ReleaseOutcome::AllReleased, RestoreOutcome::SomeFailed) => Err(Error::FanModeNotRestored),
        (Ok(()), ReleaseOutcome::AllReleased, RestoreOutcome::AllRestored) => Ok(()),
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
    let releases = app.release_failures.clone();
    let restores = app.restore_failures.clone();

    // Every zone task has ended, so every guard has released its fans. The app
    // holds the last handle of each session, so this drop restores each
    // board's fan mode before the exit status is decided.
    drop(app);

    exit_result(run, releases.outcome(), restores.outcome())
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
            fs,
            io,
            path::{Path, PathBuf},
            process,
            time::Duration,
        },
        clap::CommandFactory,
        tokio::time::timeout,
        super::{exit_result, Opt, query_sensors, RunResults, sessions_to_open, Signals, start_up},
        crate::{
            config::{Config, FanConfig, load_config, parse_config},
            error::Error,
            fan::ReleaseOutcome,
            session::RestoreOutcome,
        },
    };

    fn parse(contents: &str) -> Config {
        parse_config(contents, Path::new("/etc/coolerd.toml")).unwrap()
    }

    /// The config of `docs/examples/<name>.toml`
    fn example(name: &str) -> Config {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("docs/examples/{}.toml", name));

        load_config(&path).unwrap_or_else(|e| panic!("{}", e))
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
    fn hwmon_desktop_example_opens_no_session() {
        let config = example("hwmon-desktop");

        assert_eq!(config.zones[0].fan_list, [FanConfig::Hwmon { chip: "nct6798".into(), pwm: 2 }]);
        assert_eq!(sessions_to_open(&config), []);
    }

    #[test]
    fn nvidia_example_opens_no_session() {
        assert_eq!(sessions_to_open(&example("nvidia")), []);
    }

    #[test]
    fn supermicro_example_opens_the_default_session() {
        assert_eq!(sessions_to_open(&example("supermicro")), [("default".to_owned(), vec![0, 1])]);
    }

    #[test]
    fn mixed_example_opens_the_default_session() {
        assert_eq!(sessions_to_open(&example("mixed")), [("default".to_owned(), vec![0, 1])]);
    }

    const UPSTREAM_URL: &str = "https://github.com/chenxiaolong/ipmi-fan-control";
    const README_MAX_LINES: usize = 150;

    /// The targets of the inline markdown links of `text` that name a file:
    /// links inside fenced code and links to a URL or to a fragment of the
    /// same page are left out, and a fragment after a file is cut off
    fn file_link_targets(text: &str) -> Vec<String> {
        let mut targets = Vec::new();
        let mut in_fence = false;

        for line in text.lines() {
            if line.trim_start().starts_with("```") {
                in_fence = !in_fence;
                continue;
            }
            if in_fence {
                continue;
            }

            for rest in line.split("](").skip(1) {
                let Some((target, _)) = rest.split_once(')') else {
                    continue;
                };
                let file = target.split('#').next().unwrap_or_default();

                if file.is_empty() || file.contains("://") || file.starts_with("mailto:") {
                    continue;
                }

                targets.push(file.to_owned());
            }
        }

        targets
    }

    /// `README.md` and every `docs/*.md` page, each with its path from the
    /// repository root
    fn documentation_pages() -> Vec<PathBuf> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut pages: Vec<PathBuf> = fs::read_dir(root.join("docs"))
            .expect("the repository has a docs directory")
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
            .map(|path| path.strip_prefix(root).unwrap().to_owned())
            .collect();

        pages.sort();
        pages.insert(0, PathBuf::from("README.md"));
        pages
    }

    /// Every file of `docs/examples`, each with its path from the repository
    /// root
    fn example_files() -> Vec<PathBuf> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files: Vec<PathBuf> = fs::read_dir(root.join("docs/examples"))
            .expect("the repository has a docs/examples directory")
            .map(|entry| entry.unwrap().path())
            .map(|path| path.strip_prefix(root).unwrap().to_owned())
            .collect();

        files.sort();
        files
    }

    fn read_repo_file(relative: &Path) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);

        fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {}", path.display(), e))
    }

    #[test]
    fn file_link_targets_skips_code_urls_and_fragments() {
        let text = concat!(
            "See [the keys](docs/configuration.md#zones) and [upstream](https://example.com/a).\n",
            "```sh\n",
            "echo [not a link](missing.md)\n",
            "```\n",
            "Back to [the top](#top), then [one](a.md) and [two](../b.toml).\n",
        );

        assert_eq!(file_link_targets(text), ["docs/configuration.md", "a.md", "../b.toml"]);
    }

    #[test]
    fn documentation_links_name_existing_files() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut missing = Vec::new();

        for page in documentation_pages() {
            let directory = root.join(&page).parent().unwrap().to_owned();

            for target in file_link_targets(&read_repo_file(&page)) {
                if !directory.join(&target).exists() {
                    missing.push(format!("{}: {}", page.display(), target));
                }
            }
        }

        assert_eq!(missing, Vec::<String>::new(), "links to files that do not exist");
    }

    #[test]
    fn readme_links_every_page_and_example() {
        let pages = documentation_pages();
        let targets = file_link_targets(&read_repo_file(Path::new("README.md")));
        let mut wanted: Vec<String> = pages[1..].iter().map(|page| page.display().to_string()).collect();

        let examples = example_files();

        wanted.extend(examples.iter().map(|file| file.display().to_string()));

        // A README with no page to link would pass on an empty docs directory
        assert!(pages.len() > 1, "docs holds no page");
        assert!(!examples.is_empty(), "docs/examples holds no file");

        let unlinked: Vec<&String> = wanted.iter().filter(|page| !targets.contains(page)).collect();

        assert_eq!(unlinked, Vec::<&String>::new(), "README.md does not link these files");
    }

    #[test]
    fn every_example_config_has_a_recipe() {
        let files = example_files();
        let lone: Vec<&PathBuf> = files.iter()
            .filter(|file| !files.contains(&file.with_extension("toml")) || !files.contains(&file.with_extension("nix")))
            .collect();

        assert_eq!(lone, Vec::<&PathBuf>::new(), "an example is a .toml file and a .nix file of one name");
    }

    #[test]
    fn readme_is_a_short_entry_page_that_credits_upstream() {
        let readme = read_repo_file(Path::new("README.md"));

        assert!(readme.lines().count() <= README_MAX_LINES, "README.md has {} lines", readme.lines().count());
        assert!(readme.contains(UPSTREAM_URL));
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
        let result = exit_result(Ok(()), ReleaseOutcome::SomeFailed, RestoreOutcome::AllRestored);

        assert!(matches!(result, Err(Error::FansNotReleased)), "{:?}", result);
    }

    #[test]
    fn clean_run_with_every_fan_released_succeeds() {
        let result = exit_result(Ok(()), ReleaseOutcome::AllReleased, RestoreOutcome::AllRestored);

        assert!(result.is_ok(), "{:?}", result);
    }

    #[test]
    fn run_error_is_kept_whatever_the_releases_and_restores_did() {
        for releases in [ReleaseOutcome::SomeFailed, ReleaseOutcome::AllReleased] {
            for restores in [RestoreOutcome::SomeFailed, RestoreOutcome::AllRestored] {
                let result = exit_result(Err(Error::SensorsWithoutSession), releases, restores);

                assert!(matches!(result, Err(Error::SensorsWithoutSession)), "{:?}", result);
            }
        }
    }

    #[test]
    fn clean_run_with_a_failed_restore_is_an_error() {
        let result = exit_result(Ok(()), ReleaseOutcome::AllReleased, RestoreOutcome::SomeFailed);

        assert!(matches!(result, Err(Error::FanModeNotRestored)), "{:?}", result);
    }

    #[test]
    fn failed_release_is_reported_over_a_failed_restore() {
        let result = exit_result(Ok(()), ReleaseOutcome::SomeFailed, RestoreOutcome::SomeFailed);

        assert!(matches!(result, Err(Error::FansNotReleased)), "{:?}", result);
    }

    /// The exit result of a run that received `results` in order, with every
    /// fan released and every fan mode restored
    fn exit_after(results: Vec<super::Result<()>>) -> super::Result<()> {
        let mut run = RunResults::default();

        for result in results {
            assert!(run.add(result).is_none(), "an error was handed back to be logged");
        }

        exit_result(run.into_result(), ReleaseOutcome::AllReleased, RestoreOutcome::AllRestored)
    }

    #[test]
    fn zone_error_after_a_signal_fails_the_exit() {
        // The signal, then a zone that fails as it finishes its update
        let result = exit_after(vec![Ok(()), Err(Error::SensorsWithoutSession)]);

        assert!(matches!(result, Err(Error::SensorsWithoutSession)), "{:?}", result);
    }

    #[test]
    fn signal_stop_without_an_error_succeeds() {
        // The signal, then two cancelled zone loops
        let result = exit_after(vec![Ok(()), Ok(()), Ok(())]);

        assert!(result.is_ok(), "{:?}", result);
    }

    #[test]
    fn zone_error_before_a_signal_fails_the_exit() {
        let result = exit_after(vec![Err(Error::SensorsWithoutSession), Ok(())]);

        assert!(matches!(result, Err(Error::SensorsWithoutSession)), "{:?}", result);
    }

    #[test]
    fn error_after_the_first_error_is_handed_back_to_be_logged() {
        let mut run = RunResults::default();

        assert!(run.add(Err(Error::SensorsWithoutSession)).is_none());

        let later = run.add(Err(Error::FansNotReleased));

        assert!(matches!(later, Some(Error::FansNotReleased)), "{:?}", later);

        let result = run.into_result();

        assert!(matches!(result, Err(Error::SensorsWithoutSession)), "{:?}", result);
    }

    #[test]
    fn sensor_query_without_a_session_fails() {
        let result = query_sensors(None);

        assert!(matches!(result, Err(Error::SensorsWithoutSession)), "{:?}", result);
    }
}
