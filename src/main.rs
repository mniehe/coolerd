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
        future::Future,
        io,
        path::PathBuf,
        process,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    },
    clap::Parser,
    log::{debug, error, info, trace},
    tokio::{
        task::{self, JoinSet},
        time::sleep,
    },

    config::{Config, FanConfig, load_config, Recovery, RECOVERY_FIRST_DELAY_S, Zone},
    curve::{Transition, ZoneState},
    error::{Error, Result},
    fan::{build_fan, FanGuard, ReleaseFailures, ReleaseOutcome},
    freeipmi::SensorReading,
    hwmon::HwmonRoot,
    nvml::{worker_failed, Nvml, RealNvml},
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
    /// The failed writes of 100% and fan-mode restores of every session
    restore_failures: RestoreFailures,
    /// The zones in recovery
    recoveries: Recoveries,
    /// Whether a zone was in recovery when `run` accepted the first stop
    stop_recoveries: RecoveryOutcome,
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
            recoveries: Recoveries::default(),
            stop_recoveries: RecoveryOutcome::AllRunning,
        })
    }

    /// Run asynchronous loops for each zone. Returns when interrupted by one
    /// of `signals` (eg. ^C) or if a fatal error occurs.
    async fn run(&mut self, signals: &mut Signals) -> Result<()> {
        let mut loops = JoinSet::new();

        for (zone_config, recovery) in self.config.zones.iter().zip(&self.config.zone_recovery) {
            loops.spawn(Self::zone_loop(
                // Only a zone with a Supermicro fan has an open session
                self.sessions.get(&zone_config.session.0).cloned(),
                self.nvml.clone(),
                self.release_failures.clone(),
                ZoneHealth::new(self.recoveries.clone()),
                // Cloned since there's no structured concurrency support yet
                Arc::new(zone_config.clone()),
                *recovery,
            ));
        }

        let mut results = RunResults::default();

        loop {
            // A zone inside an update finishes it after the stop, and that
            // update can end its recovery; the stop still found it in recovery.
            // The boundary is the moment `run` accepts the stop, so each branch
            // reads the recovery outcome before it does anything else.
            let (ret, recovery): (Result<()>, RecoveryOutcome) = tokio::select! {
                // Explicitly interrupted by ^C or signal handler
                _ = signals.recv() => {
                    let recovery = self.recoveries.outcome();
                    info!("Interrupted");
                    (Ok(()), recovery)
                }
                // Oh boy, this is an Option<Result<Result<()>, JoinError>>
                r = loops.join_next() => {
                    let recovery = self.recoveries.outcome();
                    let ret = match r {
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
                    };
                    (ret, recovery)
                }
            };

            if let Some(line) = results.receive(ret, recovery) {
                error!("{}", line);
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

        self.stop_recoveries = results.stop_recovery();

        results.into_result()
    }

    /// Main loop for a zone. The loop runs while the future is being polled,
    /// and returns only an error that `recovery` does not retry.
    async fn zone_loop(
        session: Option<Arc<IpmiSession>>,
        nvml: Arc<dyn Nvml>,
        release_failures: ReleaseFailures,
        mut health: ZoneHealth,
        zone_config: Arc<Zone>,
        recovery: Recovery,
    ) -> Result<()> {
        let hwmon = HwmonRoot::default();
        let interval = zone_config.interval.to_duration();
        let mut zone = LiveZone {
            state: ZoneState::new(&zone_config),
            readers: ZoneReaders::new(hwmon.clone()),
            hwmon,
            session,
            nvml,
            zone_config,
        };

        Err(recover(&mut zone, recovery, interval, &release_failures, &mut health, sleep).await)
    }

    /// Update fan PWM duty cycle from the zone's source readings and curves
    fn update_duty_cycle(
        session: Option<&IpmiSession>,
        zone_config: &Zone,
        state: &mut ZoneState,
        readers: &mut ZoneReaders,
        fans: &mut FanGuard,
    ) -> Result<(), ZoneFailure> {
        trace!("Querying sources for fans {:?}", fans.descriptions());

        let label = zone_label(session);
        let mut query_ipmi = || query_sensors(session);
        let failed_last = state.failed_sources();
        let readings = read_zone_sources(zone_config, &failed_last, &mut query_ipmi,
            &mut |index, source| readers.read(index, source))
            .map_err(|error| ZoneFailure { step: FailedStep::Sources, error })?;
        let update = state.next_duty(zone_config, &readings);

        Self::log_transitions(label, &fans.descriptions(), zone_config, &update.transitions);

        let applied = fans.set_duty(update.duty)
            .map_err(|error| ZoneFailure { step: FailedStep::Fans, error })?;

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

/// The step of a zone's work that failed
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FailedStep {
    /// Reading the sources. A source with `on_failure = { type = "exit" }`
    /// fails here, with its own error.
    Sources,
    /// Building, taking over or driving a fan
    Fans,
}

/// An error of a zone, with the step that returned it
#[derive(Debug)]
struct ZoneFailure {
    step: FailedStep,
    error: Error,
}

/// What a zone does after a failure, once its fans are released
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AfterFailure {
    /// Wait, then take the fans over again
    Retry,
    /// Return the error, which stops every zone
    Stop,
}

/// Whether `error`, or the error a fan error wraps, means that the NVML
/// worker can make no more calls
fn nvml_worker_failed(error: &Error) -> bool {
    match error {
        Error::Fan { source, .. } => nvml_worker_failed(source),
        other => worker_failed(other),
    }
}

/// Whether a zone has completed an update since it started
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ZoneProgress {
    NeverUpdated,
    /// At least one update succeeded; this never goes back
    Updated,
}

/// Decide whether a zone retries `failure` after its fans were released with
/// `release`. Only a fan error is retried, and only under `retry` once the
/// zone has completed an update: before that a failure is a fan or a config
/// that does not work, and stopping lets the session's drop set each IPMI
/// zone to 100%. A source error is the source's `on_failure = exit`, a
/// failed NVML worker never recovers in the process, and a failed release
/// can leave a fan slow.
fn after_failure(recovery: Recovery, progress: ZoneProgress, failure: &ZoneFailure, release: ReleaseOutcome) -> AfterFailure {
    let retried = matches!(recovery, Recovery::Retry { .. })
        && progress == ZoneProgress::Updated
        && failure.step == FailedStep::Fans
        && release == ReleaseOutcome::AllReleased
        && !nvml_worker_failed(&failure.error);

    if retried {
        AfterFailure::Retry
    } else {
        AfterFailure::Stop
    }
}

/// The waits of a zone's recovery, in seconds: the first is
/// [`RECOVERY_FIRST_DELAY_S`], and each consecutive failure doubles it up to
/// the cap
#[derive(Debug)]
struct Backoff {
    next_s: u64,
    cap_s: u64,
}

impl Backoff {
    fn new(cap_s: u64) -> Self {
        Self { next_s: RECOVERY_FIRST_DELAY_S.min(cap_s), cap_s }
    }

    /// The wait after one more consecutive failure
    fn failed(&mut self) -> u64 {
        let delay_s = self.next_s;

        self.next_s = delay_s.saturating_mul(2).min(self.cap_s);

        delay_s
    }

    /// Start again from the first wait, after a successful update
    fn reset(&mut self) {
        self.next_s = RECOVERY_FIRST_DELAY_S.min(self.cap_s);
    }
}

/// The cap of the waits of `recovery`. A zone under `exit` never waits.
fn recovery_cap(recovery: Recovery) -> u64 {
    match recovery {
        Recovery::Retry { max_delay_s } => max_delay_s,
        Recovery::Exit {} => RECOVERY_FIRST_DELAY_S,
    }
}

/// The log line of a failure that the zone tagged `label` retries after
/// `delay_s` seconds
fn retry_line(label: &str, error: &Error, delay_s: u64) -> String {
    format!("Zone error: [{}] {}; released the zone's fans, taking them over again in {} seconds", label, error, delay_s)
}

/// Whether any zone is in recovery
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryOutcome {
    AllRunning,
    SomeRecovering,
}

/// The number of zones in recovery: each failed and has not completed an
/// update since. Every zone shares it, so that the exit status can tell
/// whether a stop found a zone in recovery.
#[derive(Clone, Debug, Default)]
struct Recoveries(Arc<AtomicUsize>);

impl Recoveries {
    fn outcome(&self) -> RecoveryOutcome {
        if self.0.load(Ordering::SeqCst) == 0 {
            RecoveryOutcome::AllRunning
        } else {
            RecoveryOutcome::SomeRecovering
        }
    }
}

/// The recovery outcome of a run: some zone counts as in recovery when one
/// was when `run` accepted the first stop, or still is after every zone ended
fn kept_recovery(at_stop: RecoveryOutcome, after_join: RecoveryOutcome) -> RecoveryOutcome {
    if at_stop == RecoveryOutcome::SomeRecovering {
        return at_stop;
    }

    after_join
}

/// One zone's count in [`Recoveries`]. An abort leaves a zone in recovery
/// counted, so that the stop reports it.
#[derive(Debug)]
struct ZoneHealth {
    zones: Recoveries,
    recovering: bool,
}

impl ZoneHealth {
    fn new(zones: Recoveries) -> Self {
        Self { zones, recovering: false }
    }

    /// Count the zone as in recovery, once however often it fails
    fn failed(&mut self) {
        if self.recovering {
            return;
        }

        self.recovering = true;
        self.zones.0.fetch_add(1, Ordering::SeqCst);
    }

    /// Record a successful update. Returns whether it ended a recovery.
    fn updated(&mut self) -> bool {
        if !self.recovering {
            return false;
        }

        self.recovering = false;
        self.zones.0.fetch_sub(1, Ordering::SeqCst);

        true
    }
}

/// The work of one zone that [`recover`] drives
trait ZoneSteps {
    /// The tag of the zone's log lines
    fn label(&self) -> &str;
    /// Build the zone's fans and take each over, holding it in `fans`
    fn take_over(&mut self, fans: &mut FanGuard) -> Result<()>;
    /// Read the sources and drive the fans once
    fn update(&mut self, fans: &mut FanGuard) -> Result<(), ZoneFailure>;
    /// Release every fan of `fans` and return whether each release succeeded
    fn release(&mut self, fans: FanGuard) -> ReleaseOutcome;
    /// Forget the smoothed readings of the sources, so that a take-over after
    /// a failure maps only readings taken since
    fn clear_smoothing(&mut self);
}

/// A zone of the config, on its session and NVML. The source state lasts
/// across take-overs, so that a source's runner still knows of a killed
/// program that has not exited.
struct LiveZone {
    session: Option<Arc<IpmiSession>>,
    nvml: Arc<dyn Nvml>,
    zone_config: Arc<Zone>,
    hwmon: HwmonRoot,
    state: ZoneState,
    readers: ZoneReaders,
}

/// Each step that calls a fan or a source blocks on IPMI, sysfs or NVML, so it
/// runs in `block_in_place` and leaves the worker thread to the other zones
impl ZoneSteps for LiveZone {
    fn label(&self) -> &str {
        zone_label(self.session.as_deref())
    }

    fn take_over(&mut self, fans: &mut FanGuard) -> Result<()> {
        task::block_in_place(|| {
            for fan_config in &self.zone_config.fan_list {
                fans.add(build_fan(fan_config, self.session.as_ref(), &self.hwmon, &self.nvml)?)?;
            }

            Ok::<_, Error>(())
        })?;

        info!("[{}] Starting loop for fans {:?}", self.label(), fans.descriptions());

        Ok(())
    }

    fn update(&mut self, fans: &mut FanGuard) -> Result<(), ZoneFailure> {
        task::block_in_place(|| {
            MainApp::update_duty_cycle(self.session.as_deref(), &self.zone_config, &mut self.state, &mut self.readers, fans)
        })
    }

    fn release(&mut self, fans: FanGuard) -> ReleaseOutcome {
        task::block_in_place(|| fans.release())
    }

    fn clear_smoothing(&mut self) {
        self.state.clear_smoothing();
    }
}

/// Run `zone` until a failure that `recovery` does not retry, and return its
/// error. Each attempt takes the fans over with a fresh guard and updates
/// them every `interval`; after a failure the zone releases them, and a
/// retried zone waits through `sleep` and forgets its smoothed readings
/// before the next attempt.
///
/// Each Supermicro fan holds an Arc of the session, so the guard releases the
/// zone's fans before the session's own drop restores the fan mode. A panic
/// and an abort of the task drop the guard, which releases the fans; an abort
/// takes effect once the poll in progress returns.
async fn recover<Z, S, F>(
    zone: &mut Z,
    recovery: Recovery,
    interval: Duration,
    release_failures: &ReleaseFailures,
    health: &mut ZoneHealth,
    mut sleep: S,
) -> Error
where
    Z: ZoneSteps,
    S: FnMut(Duration) -> F,
    F: Future<Output = ()>,
{
    let mut backoff = Backoff::new(recovery_cap(recovery));
    let mut progress = ZoneProgress::NeverUpdated;

    loop {
        let mut fans = FanGuard::new(release_failures.clone());
        let failure = attempt(zone, &mut fans, interval, &mut backoff, &mut progress, health, &mut sleep).await;
        let release = zone.release(fans);

        if after_failure(recovery, progress, &failure, release) == AfterFailure::Stop {
            return failure.error;
        }

        health.failed();

        let delay_s = backoff.failed();

        error!("{}", retry_line(zone.label(), &failure.error, delay_s));
        sleep(Duration::from_secs(delay_s)).await;
        zone.clear_smoothing();
    }
}

/// Take the fans of `zone` over into `fans` and update them every `interval`
/// until a step fails. A successful update marks `progress`.
async fn attempt<Z, S, F>(
    zone: &mut Z,
    fans: &mut FanGuard,
    interval: Duration,
    backoff: &mut Backoff,
    progress: &mut ZoneProgress,
    health: &mut ZoneHealth,
    sleep: &mut S,
) -> ZoneFailure
where
    Z: ZoneSteps,
    S: FnMut(Duration) -> F,
    F: Future<Output = ()>,
{
    if let Err(error) = zone.take_over(fans) {
        return ZoneFailure { step: FailedStep::Fans, error };
    }

    loop {
        if let Err(failure) = zone.update(fans) {
            return failure;
        }

        *progress = ZoneProgress::Updated;
        backoff.reset();

        if health.updated() {
            info!("[{}] Fans {:?} run again after recovery", zone.label(), fans.descriptions());
        }

        sleep(interval).await;
    }
}

/// The results of the zone loops and of the signals, as `run` receives them.
/// The first error is the result of the run; a signal or a cancelled loop
/// adds none, so an error that a zone returns after a signal still counts.
#[derive(Debug, Default)]
struct RunResults {
    first_error: Option<Error>,
    /// Whether a zone was in recovery when `run` accepted the first stop
    at_stop: Option<RecoveryOutcome>,
}

impl RunResults {
    /// Add one result with the recovery outcome read as `run` received it,
    /// and keep the outcome of the first result: that one is the stop.
    /// Returns the log line as [`RunResults::add`] does.
    fn receive(&mut self, result: Result<()>, recovery: RecoveryOutcome) -> Option<String> {
        self.at_stop.get_or_insert(recovery);

        self.add(result)
    }

    /// Whether a zone was in recovery at the stop
    fn stop_recovery(&self) -> RecoveryOutcome {
        self.at_stop.unwrap_or(RecoveryOutcome::AllRunning)
    }

    /// Add one result. Returns the log line of every error, the kept one
    /// included, so that an error reaches the log as soon as `run` receives
    /// it and not only when the process exits.
    fn add(&mut self, result: Result<()>) -> Option<String> {
        let Err(e) = result else {
            return None;
        };

        let line = format!("Zone error: {}", e);

        if self.first_error.is_none() {
            self.first_error = Some(e);
        }

        Some(line)
    }

    /// The result of the run
    fn into_result(self) -> Result<()> {
        self.first_error.map_or(Ok(()), Err)
    }
}

/// The result of the process: an error from the zone loops is kept, and a
/// clean run becomes an error when a fan failed to release, a session failed
/// to set a zone to 100% or to restore its fan mode, or a zone was in
/// recovery, in that order
fn exit_result(run: Result<()>, releases: ReleaseOutcome, restores: RestoreOutcome, recoveries: RecoveryOutcome) -> Result<()> {
    match (run, releases, restores, recoveries) {
        (Err(e), _, _, _) => Err(e),
        (Ok(()), ReleaseOutcome::SomeFailed, _, _) => Err(Error::FansNotReleased),
        (Ok(()), ReleaseOutcome::AllReleased, RestoreOutcome::SomeFailed, _) => Err(Error::SessionsNotRestored),
        (Ok(()), ReleaseOutcome::AllReleased, RestoreOutcome::AllRestored, RecoveryOutcome::SomeRecovering) => Err(Error::ZonesInRecovery),
        (Ok(()), ReleaseOutcome::AllReleased, RestoreOutcome::AllRestored, RecoveryOutcome::AllRunning) => Ok(()),
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

    finish(run, app)
}

/// Drop `app` once its run has ended, then decide the result of the process
fn finish(run: Result<()>, app: MainApp) -> Result<()> {
    let releases = app.release_failures.clone();
    let restores = app.restore_failures.clone();
    let recoveries = app.recoveries.clone();
    let at_stop = app.stop_recoveries;

    // Every zone task has ended, so every guard has released its fans. The app
    // holds the last handle of each session, so this drop sets each session's
    // zones to 100% and restores its fan mode before the exit status is
    // decided.
    drop(app);

    exit_result(run, releases.outcome(), restores.outcome(), kept_recovery(at_stop, recoveries.outcome()))
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
            collections::HashMap,
            fs,
            io,
            path::{Path, PathBuf},
            collections::VecDeque,
            future::Future,
            pin::{pin, Pin},
            process,
            sync::{Arc, Mutex},
            task::{Context, Poll, Waker},
            time::Duration,
        },
        clap::CommandFactory,
        tokio::time::timeout,
        super::{
            after_failure,
            AfterFailure,
            Backoff,
            exit_result,
            FailedStep,
            finish,
            kept_recovery,
            LiveZone,
            MainApp,
            Opt,
            query_sensors,
            Recoveries,
            RecoveryOutcome,
            recover,
            retry_line,
            RunResults,
            sessions_to_open,
            Signals,
            start_up,
            ZoneFailure,
            ZoneHealth,
            ZoneProgress,
            ZoneSteps,
        },
        crate::{
            command::testing::TempDir,
            config::{Config, FanConfig, load_config, parse_config, Recovery},
            curve::ZoneState,
            error::Error,
            fan::{
                fake::{CallLog, FakeFan, Op},
                Applied,
                Fan,
                FanGuard,
                ReleaseFailures,
                ReleaseOutcome,
            },
            hwmon::HwmonRoot,
            ipmi::FanMode,
            nvml::fake::FakeNvml,
            session::{
                fake::{BoardLog, FakeBoard, PortCall},
                IpmiSession,
                RestoreFailures,
                RestoreOutcome,
            },
            source::ZoneReaders,
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
        let result = exit_result(Ok(()), ReleaseOutcome::SomeFailed, RestoreOutcome::AllRestored, RecoveryOutcome::AllRunning);

        assert!(matches!(result, Err(Error::FansNotReleased)), "{:?}", result);
    }

    #[test]
    fn clean_run_with_every_fan_released_succeeds() {
        let result = exit_result(Ok(()), ReleaseOutcome::AllReleased, RestoreOutcome::AllRestored, RecoveryOutcome::AllRunning);

        assert!(result.is_ok(), "{:?}", result);
    }

    #[test]
    fn run_error_is_kept_whatever_the_releases_and_restores_did() {
        for releases in [ReleaseOutcome::SomeFailed, ReleaseOutcome::AllReleased] {
            for restores in [RestoreOutcome::SomeFailed, RestoreOutcome::AllRestored] {
                let result = exit_result(Err(Error::SensorsWithoutSession), releases, restores, RecoveryOutcome::AllRunning);

                assert!(matches!(result, Err(Error::SensorsWithoutSession)), "{:?}", result);
            }
        }
    }

    #[test]
    fn clean_run_with_a_failed_restore_is_an_error() {
        let result = exit_result(Ok(()), ReleaseOutcome::AllReleased, RestoreOutcome::SomeFailed, RecoveryOutcome::AllRunning);

        assert!(matches!(result, Err(Error::SessionsNotRestored)), "{:?}", result);
    }

    /// A signal can cancel a zone task before its fan guard exists, so the
    /// session's drop then makes the only write of 100% to the zone
    #[test]
    fn failed_full_duty_write_without_a_fan_guard_fails_the_exit() {
        let calls = BoardLog::default();
        let board = FakeBoard {
            failing: Some(PortCall::SetDutyCycle(0, 100)),
            ..FakeBoard::new(FanMode::Full, &calls)
        };
        let failures = RestoreFailures::default();
        let session = IpmiSession::with_port("test", Box::new(board), [0], failures.clone()).unwrap();

        drop(session);

        assert_eq!(failures.outcome(), RestoreOutcome::SomeFailed);

        let result = exit_result(Ok(()), ReleaseOutcome::AllReleased, failures.outcome(), RecoveryOutcome::AllRunning);

        assert!(matches!(result, Err(Error::SessionsNotRestored)), "{:?}", result);
    }

    /// An app whose one session runs over `board`, after a run that ended
    /// with no error
    fn app_over(board: FakeBoard) -> MainApp {
        let restore_failures = RestoreFailures::default();
        let session = IpmiSession::with_port("default", Box::new(board), [0], restore_failures.clone()).unwrap();

        MainApp {
            config: parse(concat!(
                "[[zones]]\n",
                "fans = [{ type = \"supermicro\", zone = 0 }]\n",
                "sources = [{ type = \"ipmi\", sensor = \"CPU Temp\" }]\n",
                "steps = [{ temp = 40, dcycle = 40 }]\n",
            )),
            sessions: HashMap::from([("default".to_owned(), Arc::new(session))]),
            nvml: Arc::new(FakeNvml::default()),
            release_failures: ReleaseFailures::default(),
            restore_failures,
            recoveries: Recoveries::default(),
            stop_recoveries: RecoveryOutcome::AllRunning,
        }
    }

    #[test]
    fn finish_fails_on_a_mode_restore_that_fails_as_the_app_drops() {
        let calls = BoardLog::default();
        let board = FakeBoard {
            failing: Some(PortCall::SetFanMode(FanMode::Standard)),
            ..FakeBoard::new(FanMode::Standard, &calls)
        };

        let result = finish(Ok(()), app_over(board));

        assert!(calls.lock().unwrap().ends_with(&[PortCall::SetDutyCycle(0, 100), PortCall::SetFanMode(FanMode::Standard)]));
        assert!(matches!(result, Err(Error::SessionsNotRestored)), "{:?}", result);
    }

    #[test]
    fn finish_fails_on_a_full_duty_write_that_fails_as_the_app_drops() {
        let calls = BoardLog::default();
        let board = FakeBoard {
            failing: Some(PortCall::SetDutyCycle(0, 100)),
            ..FakeBoard::new(FanMode::Full, &calls)
        };

        let result = finish(Ok(()), app_over(board));

        assert!(calls.lock().unwrap().ends_with(&[PortCall::SetDutyCycle(0, 100)]));
        assert!(matches!(result, Err(Error::SessionsNotRestored)), "{:?}", result);
    }

    #[test]
    fn finish_succeeds_when_the_app_drops_cleanly() {
        let calls = BoardLog::default();
        let result = finish(Ok(()), app_over(FakeBoard::new(FanMode::Standard, &calls)));

        assert!(result.is_ok(), "{:?}", result);
    }

    #[test]
    fn failed_release_is_reported_over_a_failed_restore() {
        let result = exit_result(Ok(()), ReleaseOutcome::SomeFailed, RestoreOutcome::SomeFailed, RecoveryOutcome::AllRunning);

        assert!(matches!(result, Err(Error::FansNotReleased)), "{:?}", result);
    }

    /// The exit result of a run that received `results` in order, with every
    /// fan released and every fan mode restored
    fn exit_after(results: Vec<super::Result<()>>) -> super::Result<()> {
        let mut run = RunResults::default();

        for result in results {
            let failed = result.is_err();

            assert_eq!(run.add(result).is_some(), failed, "only an error is handed back to be logged");
        }

        exit_result(run.into_result(), ReleaseOutcome::AllReleased, RestoreOutcome::AllRestored, RecoveryOutcome::AllRunning)
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
    fn every_zone_error_is_handed_back_to_be_logged() {
        let mut run = RunResults::default();

        let first = run.add(Err(Error::SensorsWithoutSession));
        let later = run.add(Err(Error::FansNotReleased));

        assert_eq!(first, Some(format!("Zone error: {}", Error::SensorsWithoutSession)));
        assert_eq!(later, Some(format!("Zone error: {}", Error::FansNotReleased)));

        let result = run.into_result();

        assert!(matches!(result, Err(Error::SensorsWithoutSession)), "{:?}", result);
    }

    #[test]
    fn sensor_query_without_a_session_fails() {
        let result = query_sensors(None);

        assert!(matches!(result, Err(Error::SensorsWithoutSession)), "{:?}", result);
    }

    const RETRY_300: Recovery = Recovery::Retry { max_delay_s: 300 };

    #[test]
    fn recovery_delay_doubles_to_the_cap_and_resets() {
        let mut backoff = Backoff::new(300);
        let delays: Vec<u64> = (0..7).map(|_| backoff.failed()).collect();

        assert_eq!(delays, [10, 20, 40, 80, 160, 300, 300]);

        backoff.reset();

        assert_eq!(backoff.failed(), 10);

        let mut shortest = Backoff::new(10);
        let delays: Vec<u64> = (0..4).map(|_| shortest.failed()).collect();

        assert_eq!(delays, [10, 10, 10, 10]);

        // Doubling saturates, so no cap can overflow it
        let mut unbounded = Backoff::new(u64::MAX);

        for _ in 0..70 {
            unbounded.failed();
        }

        assert_eq!(unbounded.failed(), u64::MAX);
    }

    fn failure(step: FailedStep, error: Error) -> ZoneFailure {
        ZoneFailure { step, error }
    }

    /// An error with the same variant whichever step returns it
    fn plain_error() -> Error {
        Error::SensorNotFound("CPU Temp".to_owned())
    }

    /// `source` as a fan guard returns it
    fn fan_error(source: Error) -> Error {
        Error::Fan { fan: "nvidia gpu 00000000:01:00.0 fan 0".to_owned(), source: Box::new(source) }
    }

    fn stopped_worker() -> Error {
        Error::NvmlNoReply { gpu: "00000000:01:00.0".to_owned(), operation: "set_fan_speed" }
    }

    #[test]
    fn only_a_fan_error_with_a_clean_release_is_retried() {
        let exit = Recovery::Exit {};
        let updated = ZoneProgress::Updated;

        assert_eq!(after_failure(RETRY_300, updated, &failure(FailedStep::Fans, plain_error()), ReleaseOutcome::AllReleased), AfterFailure::Retry);
        assert_eq!(after_failure(RETRY_300, updated, &failure(FailedStep::Fans, fan_error(plain_error())), ReleaseOutcome::AllReleased), AfterFailure::Retry);
        assert_eq!(after_failure(exit, updated, &failure(FailedStep::Fans, plain_error()), ReleaseOutcome::AllReleased), AfterFailure::Stop);

        // A zone that has never completed an update stops on a fan error too
        let never = ZoneProgress::NeverUpdated;

        assert_eq!(after_failure(RETRY_300, never, &failure(FailedStep::Fans, plain_error()), ReleaseOutcome::AllReleased), AfterFailure::Stop);
        assert_eq!(after_failure(RETRY_300, never, &failure(FailedStep::Fans, fan_error(plain_error())), ReleaseOutcome::AllReleased), AfterFailure::Stop);

        for recovery in [RETRY_300, exit] {
            let stops = [
                (failure(FailedStep::Sources, plain_error()), ReleaseOutcome::AllReleased),
                (failure(FailedStep::Fans, fan_error(stopped_worker())), ReleaseOutcome::AllReleased),
                (failure(FailedStep::Fans, stopped_worker()), ReleaseOutcome::AllReleased),
                (failure(FailedStep::Fans, plain_error()), ReleaseOutcome::SomeFailed),
            ];

            for (failure, release) in stops {
                assert_eq!(after_failure(recovery, updated, &failure, release), AfterFailure::Stop, "{:?} {:?} {:?}", recovery, failure, release);
            }
        }
    }

    #[test]
    fn retry_line_names_the_error_and_the_delay() {
        let error = fan_error(plain_error());
        let line = retry_line("bmc", &error, 40);

        assert!(line.starts_with("Zone error: [bmc] "), "{}", line);
        assert!(line.contains(&error.to_string()), "{}", line);
        assert!(line.contains("40 seconds"), "{}", line);
    }

    #[test]
    fn recovery_seen_at_the_stop_is_kept() {
        use RecoveryOutcome::{AllRunning, SomeRecovering};

        // The zone was in recovery as the stop came, then finished an update
        assert_eq!(kept_recovery(SomeRecovering, AllRunning), SomeRecovering);
        assert_eq!(kept_recovery(AllRunning, SomeRecovering), SomeRecovering);
        assert_eq!(kept_recovery(AllRunning, AllRunning), AllRunning);

        let calls = BoardLog::default();
        let app = MainApp { stop_recoveries: SomeRecovering, ..app_over(FakeBoard::new(FanMode::Standard, &calls)) };
        let result = finish(Ok(()), app);

        assert!(matches!(result, Err(Error::ZonesInRecovery)), "{:?}", result);

        let result = finish(Ok(()), app_over(FakeBoard::new(FanMode::Standard, &calls)));

        assert!(result.is_ok(), "{:?}", result);
    }

    #[test]
    fn stop_snapshot_is_the_recovery_state_at_the_first_result() {
        use RecoveryOutcome::{AllRunning, SomeRecovering};

        // The signal found a zone in recovery, which then finished an update
        // before the cancelled loops came back
        let mut run = RunResults::default();

        assert_eq!(run.receive(Ok(()), SomeRecovering), None);
        assert_eq!(run.receive(Ok(()), AllRunning), None);
        assert_eq!(run.stop_recovery(), SomeRecovering);

        // The stop found every zone running, and a later result found one in
        // recovery: the stop keeps its own outcome, and the outcome after the
        // join still reports the later recovery
        let mut run = RunResults::default();
        let line = run.receive(Ok(()), AllRunning);
        let later = run.receive(Err(Error::SensorsWithoutSession), SomeRecovering);

        assert_eq!(line, None);
        assert_eq!(later, Some(format!("Zone error: {}", Error::SensorsWithoutSession)));
        assert_eq!(run.stop_recovery(), AllRunning);
        assert_eq!(kept_recovery(run.stop_recovery(), SomeRecovering), SomeRecovering);

        let result = run.into_result();

        assert!(matches!(result, Err(Error::SensorsWithoutSession)), "{:?}", result);
        assert_eq!(RunResults::default().stop_recovery(), AllRunning, "a run with no result found no zone in recovery");
    }

    /// A fan that records each duty it is driven at
    struct DutyFan(Arc<Mutex<Vec<u8>>>);

    impl Fan for DutyFan {
        fn describe(&self) -> String {
            "duty".to_owned()
        }

        fn take_control(&mut self) -> super::Result<()> {
            Ok(())
        }

        fn set_duty(&mut self, duty: u8) -> super::Result<Applied> {
            self.0.lock().unwrap().push(duty);

            Ok(Applied { before: None, sent: Some(duty) })
        }

        fn release(&mut self) -> super::Result<()> {
            Ok(())
        }
    }

    /// `LiveZone::update` runs in `block_in_place`, which needs a multi-thread
    /// runtime
    #[tokio::test(flavor = "multi_thread")]
    async fn live_zone_clear_drops_its_smoothing_samples() {
        let dir = TempDir::new();
        let temp = dir.write("temp", "40000");
        let config = parse(&format!(concat!(
            "[[zones]]\n",
            "ipmi_zones = [0]\n",
            "sources = [{{ type = \"file\", path = {:?}, smoothing_samples = 4 }}]\n",
            "steps = [{{ temp = 40, dcycle = 40 }}, {{ temp = 80, dcycle = 100 }}]\n",
        ), temp));
        let zone_config = Arc::new(config.zones[0].clone());
        let hwmon = HwmonRoot::default();
        let mut zone = LiveZone {
            session: None,
            nvml: Arc::new(FakeNvml::default()),
            state: ZoneState::new(&zone_config),
            readers: ZoneReaders::new(hwmon.clone()),
            hwmon,
            zone_config,
        };
        let duties = Arc::new(Mutex::new(Vec::new()));
        let mut fans = FanGuard::new(ReleaseFailures::default());

        fans.add(Box::new(DutyFan(duties.clone()))).unwrap();

        for _ in 0..3 {
            zone.update(&mut fans).unwrap();
        }

        ZoneSteps::clear_smoothing(&mut zone);
        fs::write(&temp, "80000").unwrap();
        zone.update(&mut fans).unwrap();

        // Without the clear, the mean of 40, 40, 40 and 80 would give 55%
        assert_eq!(*duties.lock().unwrap(), [40, 40, 40, 100]);
    }

    /// A zone of one `ipmi` source with `source_keys` and no retries, with no
    /// session to query
    fn sessionless_zone(source_keys: &str) -> Config {
        parse(&format!(concat!(
            "[[zones]]\n",
            "retries = 0\n",
            "ipmi_zones = [0]\n",
            "sources = [{{ type = \"ipmi\", sensor = \"CPU Temp\"{} }}]\n",
            "steps = [{{ temp = 40, dcycle = 40 }}]\n",
        ), source_keys))
    }

    /// Run one update of the zone of `config` with no session over `fan`
    fn update_once(config: &Config, fan: FakeFan) -> super::Result<(), ZoneFailure> {
        let zone = &config.zones[0];
        let mut state = ZoneState::new(zone);
        let mut readers = ZoneReaders::new(HwmonRoot::default());
        let mut fans = FanGuard::new(ReleaseFailures::default());

        fans.add(Box::new(fan)).unwrap();

        MainApp::update_duty_cycle(None, zone, &mut state, &mut readers, &mut fans)
    }

    #[test]
    fn update_tags_a_source_error_and_a_fan_error() {
        let calls = CallLog::default();
        let failing_fan = || FakeFan { failing: Some(Op::SetDuty), ..FakeFan::new("a", &calls) };

        // The default `on_failure = exit` returns the failed query's error
        let source = update_once(&sessionless_zone(""), failing_fan()).unwrap_err();

        assert_eq!(source.step, FailedStep::Sources, "{:?}", source);
        assert!(!log_of(&calls).contains(&("a", Op::SetDuty)), "a source error drives no fan");

        // Under `ignore` the zone runs at 100% and the fan refuses it
        let fan = update_once(&sessionless_zone(", on_failure = { type = \"ignore\" }"), failing_fan()).unwrap_err();

        assert_eq!(fan.step, FailedStep::Fans, "{:?}", fan);
        assert!(matches!(&fan.error, Error::Fan { fan, .. } if fan == "a"), "{:?}", fan);
    }

    #[test]
    fn stop_with_a_zone_in_recovery_is_an_error() {
        let recoveries = Recoveries::default();
        let mut running = ZoneHealth::new(recoveries.clone());
        let mut failing = ZoneHealth::new(recoveries.clone());

        running.failed();
        running.updated();
        failing.failed();

        let result = exit_result(Ok(()), ReleaseOutcome::AllReleased, RestoreOutcome::AllRestored, recoveries.outcome());

        assert!(matches!(result, Err(Error::ZonesInRecovery)), "{:?}", result);

        let run_error = exit_result(Err(Error::SensorsWithoutSession), ReleaseOutcome::AllReleased, RestoreOutcome::AllRestored, RecoveryOutcome::SomeRecovering);
        let release = exit_result(Ok(()), ReleaseOutcome::SomeFailed, RestoreOutcome::AllRestored, RecoveryOutcome::SomeRecovering);
        let restore = exit_result(Ok(()), ReleaseOutcome::AllReleased, RestoreOutcome::SomeFailed, RecoveryOutcome::SomeRecovering);

        assert!(matches!(run_error, Err(Error::SensorsWithoutSession)), "{:?}", run_error);
        assert!(matches!(release, Err(Error::FansNotReleased)), "{:?}", release);
        assert!(matches!(restore, Err(Error::SessionsNotRestored)), "{:?}", restore);
    }

    #[test]
    fn stop_after_a_zone_recovered_succeeds() {
        let recoveries = Recoveries::default();
        let mut health = ZoneHealth::new(recoveries.clone());

        health.failed();
        health.failed();

        assert!(health.updated(), "the first update after a failure ends the recovery");
        assert!(!health.updated(), "a later update ends none");

        let result = exit_result(Ok(()), ReleaseOutcome::AllReleased, RestoreOutcome::AllRestored, recoveries.outcome());

        assert!(result.is_ok(), "{:?}", result);
    }

    /// The duty that a fake zone drives its fans at
    const FAKE_DUTY: u8 = 50;

    /// The update interval of a fake zone
    const FAKE_INTERVAL: Duration = Duration::from_secs(5);

    /// A step of [`ZoneSteps`] that a fake zone ran
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ZoneStep {
        TakeOver,
        Update,
        Release,
        ClearSmoothing,
    }

    /// A zone whose take-overs hand out fake fans, one list per take-over in
    /// order, and whose updates drive them at [`FAKE_DUTY`]. It records each
    /// step it runs.
    struct FakeZone {
        takeovers: VecDeque<Vec<FakeFan>>,
        /// The updates, counted from 0 across take-overs, that fail with a fan
        /// error before they drive the fans
        failing_updates: Vec<usize>,
        updates: usize,
        /// What `release` reports in place of the guard's own outcome
        reported_release: Option<ReleaseOutcome>,
        steps: Vec<ZoneStep>,
    }

    impl FakeZone {
        fn new(takeovers: Vec<Vec<FakeFan>>) -> Self {
            Self { takeovers: takeovers.into(), failing_updates: Vec::new(), updates: 0, reported_release: None, steps: Vec::new() }
        }

        /// A zone whose updates numbered in `failing_updates` fail
        fn failing(takeovers: Vec<Vec<FakeFan>>, failing_updates: &[usize]) -> Self {
            Self { failing_updates: failing_updates.to_vec(), ..Self::new(takeovers) }
        }
    }

    impl ZoneSteps for FakeZone {
        fn label(&self) -> &str {
            "fake"
        }

        fn take_over(&mut self, fans: &mut FanGuard) -> super::Result<()> {
            self.steps.push(ZoneStep::TakeOver);

            for fan in self.takeovers.pop_front().expect("a take-over is left") {
                fans.add(Box::new(fan))?;
            }

            Ok(())
        }

        fn update(&mut self, fans: &mut FanGuard) -> super::Result<(), ZoneFailure> {
            let number = self.updates;

            self.steps.push(ZoneStep::Update);
            self.updates += 1;

            if self.failing_updates.contains(&number) {
                return Err(ZoneFailure { step: FailedStep::Fans, error: fan_error(plain_error()) });
            }

            fans.set_duty(FAKE_DUTY)
                .map(|_| ())
                .map_err(|error| ZoneFailure { step: FailedStep::Fans, error })
        }

        fn release(&mut self, fans: FanGuard) -> ReleaseOutcome {
            self.steps.push(ZoneStep::Release);

            let outcome = fans.release();

            self.reported_release.unwrap_or(outcome)
        }

        fn clear_smoothing(&mut self) {
            self.steps.push(ZoneStep::ClearSmoothing);
        }
    }

    type FakeSleep = Pin<Box<dyn Future<Output = ()>>>;

    /// A sleep that records each wait into `waits`, returns at once for the
    /// first `ready` waits and never returns from the next one
    fn sleeper(waits: &RefCell<Vec<Duration>>, ready: usize) -> impl FnMut(Duration) -> FakeSleep + '_ {
        move |duration| {
            let mut waits = waits.borrow_mut();

            waits.push(duration);

            if waits.len() <= ready {
                Box::pin(std::future::ready(()))
            } else {
                Box::pin(std::future::pending())
            }
        }
    }

    /// Poll `future` once, then drop it as an abort of its task would
    fn poll_once<F: Future>(future: F) -> Poll<F::Output> {
        let mut future = pin!(future);

        future.as_mut().poll(&mut Context::from_waker(Waker::noop()))
    }

    /// What a recovery loop did in one poll
    struct Polled {
        result: Poll<Error>,
        waits: Vec<Duration>,
        recoveries: RecoveryOutcome,
        releases: ReleaseOutcome,
    }

    /// Poll the recovery loop of `zone` under retry once, with sleeps that
    /// return at once for the first `ready` waits
    fn poll_recovery(zone: &mut FakeZone, ready: usize) -> Polled {
        let waits = RefCell::new(Vec::new());
        let recoveries = Recoveries::default();
        let failures = ReleaseFailures::default();
        let mut health = ZoneHealth::new(recoveries.clone());
        let result = poll_once(recover(zone, RETRY_300, FAKE_INTERVAL, &failures, &mut health, sleeper(&waits, ready)));

        Polled { result, waits: waits.into_inner(), recoveries: recoveries.outcome(), releases: failures.outcome() }
    }

    fn log_of(calls: &CallLog) -> Vec<(&'static str, Op)> {
        calls.lock().unwrap().clone()
    }

    /// A wait of `seconds` seconds, as a retry waits
    fn secs(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    #[test]
    fn failure_before_the_first_update_is_fatal() {
        for failing in [Op::TakeControl, Op::SetDuty] {
            let calls = CallLog::default();
            let mut zone = FakeZone::new(vec![
                vec![FakeFan { failing: Some(failing), ..FakeFan::new("a", &calls) }],
                vec![FakeFan::new("b", &calls)],
            ]);

            let polled = poll_recovery(&mut zone, 3);

            assert!(matches!(&polled.result, Poll::Ready(Error::Fan { fan, .. }) if fan == "a"), "{:?}: {:?}", failing, polled.result);
            assert_eq!(polled.waits, [], "{:?}", failing);
            assert_eq!(log_of(&calls).last(), Some(&("a", Op::Release)), "{:?}", failing);
            assert_eq!(polled.recoveries, RecoveryOutcome::AllRunning, "{:?}", failing);
        }
    }

    #[test]
    fn failed_zone_releases_its_fans_and_takes_over_again() {
        let calls = CallLog::default();
        let mut zone = FakeZone::failing(vec![
            vec![FakeFan::new("first", &calls)],
            vec![FakeFan::new("second", &calls)],
        ], &[1]);

        let polled = poll_recovery(&mut zone, 3);

        assert!(polled.result.is_pending(), "the loop returned {:?}", polled.result);
        assert_eq!(polled.waits, [FAKE_INTERVAL, secs(10), FAKE_INTERVAL, FAKE_INTERVAL]);
        assert_eq!(log_of(&calls), [
            ("first", Op::TakeControl), ("first", Op::SetDuty), ("first", Op::Release),
            ("second", Op::TakeControl), ("second", Op::SetDuty), ("second", Op::SetDuty),
            // Dropping the waiting loop, as an abort does, releases the fans
            ("second", Op::Release),
        ]);
        // The smoothing is cleared once, after the retry's wait and before its
        // take-over, and never before the first take-over
        assert_eq!(zone.steps, [
            ZoneStep::TakeOver, ZoneStep::Update, ZoneStep::Update, ZoneStep::Release,
            ZoneStep::ClearSmoothing, ZoneStep::TakeOver, ZoneStep::Update, ZoneStep::Update,
        ]);
        assert_eq!(polled.recoveries, RecoveryOutcome::AllRunning);
        assert_eq!(polled.releases, ReleaseOutcome::AllReleased);
    }

    #[test]
    fn failed_takeover_is_retried_and_failed_release_is_fatal() {
        let calls = CallLog::default();
        let mut zone = FakeZone::failing(vec![
            vec![FakeFan::new("first", &calls)],
            vec![FakeFan { failing: Some(Op::TakeControl), ..FakeFan::new("second", &calls) }],
            vec![FakeFan::new("third", &calls)],
        ], &[1]);

        let polled = poll_recovery(&mut zone, 3);

        assert!(polled.result.is_pending(), "the loop returned {:?}", polled.result);
        assert_eq!(polled.waits, [FAKE_INTERVAL, secs(10), secs(20), FAKE_INTERVAL]);
        assert_eq!(log_of(&calls), [
            ("first", Op::TakeControl), ("first", Op::SetDuty), ("first", Op::Release),
            ("second", Op::TakeControl), ("second", Op::Release),
            ("third", Op::TakeControl), ("third", Op::SetDuty), ("third", Op::Release),
        ]);

        let calls = CallLog::default();
        let mut zone = FakeZone::failing(vec![vec![
            FakeFan::new("a", &calls),
            FakeFan { failing: Some(Op::Release), ..FakeFan::new("b", &calls) },
        ]], &[1]);

        let polled = poll_recovery(&mut zone, 3);

        assert!(matches!(&polled.result, Poll::Ready(Error::Fan { .. })), "{:?}", polled.result);
        assert_eq!(polled.waits, [FAKE_INTERVAL]);
        assert_eq!(polled.releases, ReleaseOutcome::SomeFailed);
        assert_eq!(log_of(&calls), [
            ("a", Op::TakeControl), ("b", Op::TakeControl), ("a", Op::SetDuty), ("b", Op::SetDuty),
            ("b", Op::Release), ("a", Op::Release),
        ]);
    }

    #[test]
    fn recover_releases_through_the_zone_step() {
        let calls = CallLog::default();
        let mut zone = FakeZone::failing(vec![vec![FakeFan::new("a", &calls)], vec![FakeFan::new("b", &calls)]], &[1]);

        let polled = poll_recovery(&mut zone, 1);

        assert!(polled.result.is_pending(), "the loop returned {:?}", polled.result);
        assert_eq!(zone.steps.iter().filter(|step| **step == ZoneStep::Release).count(), 1);

        // The fans release cleanly, but the step's report decides
        let calls = CallLog::default();
        let mut zone = FakeZone {
            reported_release: Some(ReleaseOutcome::SomeFailed),
            ..FakeZone::failing(vec![vec![FakeFan::new("a", &calls)], vec![FakeFan::new("b", &calls)]], &[1])
        };

        let polled = poll_recovery(&mut zone, 3);

        assert!(matches!(&polled.result, Poll::Ready(Error::Fan { .. })), "{:?}", polled.result);
        assert_eq!(polled.waits, [FAKE_INTERVAL]);
        assert_eq!(zone.steps, [ZoneStep::TakeOver, ZoneStep::Update, ZoneStep::Update, ZoneStep::Release]);
        assert_eq!(log_of(&calls).last(), Some(&("a", Op::Release)));
        assert_eq!(polled.releases, ReleaseOutcome::AllReleased);
    }

    #[test]
    fn zone_waiting_to_retry_counts_as_recovering() {
        let calls = CallLog::default();
        let mut zone = FakeZone::failing(vec![vec![FakeFan::new("a", &calls)], vec![FakeFan::new("b", &calls)]], &[1]);

        let polled = poll_recovery(&mut zone, 1);

        assert!(polled.result.is_pending(), "the loop returned {:?}", polled.result);
        assert_eq!(polled.waits, [FAKE_INTERVAL, secs(10)]);
        assert_eq!(polled.recoveries, RecoveryOutcome::SomeRecovering);
    }

    #[test]
    fn recover_doubles_the_wait_and_resets_after_an_update() {
        let calls = CallLog::default();
        let mut zone = FakeZone::failing(vec![
            vec![FakeFan::new("a", &calls)],
            vec![FakeFan::new("b", &calls)],
            vec![FakeFan::new("c", &calls)],
        ], &[1, 2, 4]);

        let polled = poll_recovery(&mut zone, 4);

        assert!(polled.result.is_pending(), "the loop returned {:?}", polled.result);
        // Between the update intervals the retries wait 10, 20, then 10 again
        assert_eq!(polled.waits, [FAKE_INTERVAL, secs(10), secs(20), FAKE_INTERVAL, secs(10)]);
        // One clear per retry, each before that retry's take-over
        assert_eq!(zone.steps, [
            ZoneStep::TakeOver, ZoneStep::Update, ZoneStep::Update, ZoneStep::Release,
            ZoneStep::ClearSmoothing, ZoneStep::TakeOver, ZoneStep::Update, ZoneStep::Release,
            ZoneStep::ClearSmoothing, ZoneStep::TakeOver, ZoneStep::Update, ZoneStep::Update, ZoneStep::Release,
        ]);
    }
}
