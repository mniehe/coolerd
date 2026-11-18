mod bindings;
mod command;
mod config;
mod curve;
mod error;
mod fan;
mod freeipmi;
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
    session::IpmiSession,
    source::{read_zone_sources, ZoneReaders},
};

static LOGGING_INITIALIZED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
async fn interrupted() -> io::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;

    tokio::select! {
        _ = sigint.recv() => {}
        _ = sigterm.recv() => {}
    }

    Ok(())
}

#[cfg(windows)]
async fn interrupted() -> io::Result<()> {
    use tokio::signal::windows::{ctrl_break, ctrl_c};

    tokio::select! {
        _ = ctrl_break() => {},
        _ = ctrl_c() => {},
    }

    Ok(())
}

struct MainApp {
    config: Config,
    sessions: HashMap<String, Arc<IpmiSession>>,
}

impl MainApp {
    fn new(config: Config) -> Result<Self> {
        let mut sessions = HashMap::new();

        for (name, st) in &config.sessions.0 {
            let restore_zones: Vec<_> = config.zones
                .iter()
                .filter(|z| &z.session.0 == name)
                .flat_map(|z| &z.fan_list)
                .map(|fan| match fan {
                    FanConfig::Supermicro { zone } => *zone,
                })
                .collect();

            // Don't waste resources if nothing would use the session
            if restore_zones.is_empty() {
                continue;
            }

            sessions.insert(name.clone(), Arc::new(
                IpmiSession::new(name, &st.0, restore_zones)?));
        }

        Ok(Self {
            config,
            sessions,
        })
    }

    /// Run asynchronous loops for each zone. Returns when interrupted via
    /// signal handlers (eg. ^C) or if a fatal error occurs.
    async fn run(&mut self) -> Result<()> {
        let mut loops = JoinSet::new();

        for zone_config in &self.config.zones {
            loops.spawn(Self::zone_loop(
                self.sessions.get_mut(&zone_config.session.0).unwrap().clone(),
                // Cloned since there's no structured concurrency support yet
                Arc::new(zone_config.clone()),
            ));
        }

        let mut first_result = None;

        loop {
            let ret: Result<()> = tokio::select! {
                // Explicitly interrupted by ^C or signal handler
                c = interrupted() => {
                    if c.is_ok() {
                        info!("Interrupted");
                    }
                    c.map_err(|e| Error::Io { path: "(interrupt)".into(), source: e })
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
    ///
    /// All communication with the IPMI is behind a mutex to avoid needing
    /// multiple IPMI sessions.
    async fn zone_loop(
        session: Arc<IpmiSession>,
        zone_config: Arc<Zone>,
    ) -> Result<()> {
        info!("[{}] Starting loop for IPMI zones {:?}",
              session.name(), zone_config.fan_list);

        let mut state = ZoneState::new(&zone_config);
        let mut readers = ZoneReaders::default();

        loop {
            let s = session.clone();
            let z = zone_config.clone();
            let st = &mut state;
            let r = &mut readers;

            task::block_in_place(move || {
                Self::update_duty_cycle(s, z.as_ref(), st, r)
            })?;

            sleep(zone_config.interval.to_duration()).await;
        }
    }

    /// Update fan PWM duty cycle from the zone's source readings and curves
    fn update_duty_cycle(
        session: Arc<IpmiSession>,
        zone_config: &Zone,
        state: &mut ZoneState,
        readers: &mut ZoneReaders,
    ) -> Result<()> {
        trace!("Querying sources for zones {:?}", zone_config.fan_list);

        let mut query_ipmi = || session.lock().get_temperature_readings();
        let failed_last = state.failed_sources();
        let readings = read_zone_sources(zone_config, &failed_last, &mut query_ipmi,
            &mut |index, source| readers.read(index, source))?;
        let update = state.next_duty(zone_config, &readings);
        let dcycle_new = update.duty;

        Self::log_transitions(session.name(), zone_config, &update.transitions);

        let mut ipmi_lock = session.lock();

        for FanConfig::Supermicro { zone: z } in &zone_config.fan_list {
            let dcycle_cur = ipmi_lock.get_duty_cycle(*z)?;

            debug!("[{}] Zone {}: smoothed_temps={:?}C, curve_temps={:?}C, dcycle_cur={}%, dcycle_new={}%",
                   session.name(), z, update.smoothed_temps, update.curve_temps, dcycle_cur, dcycle_new);

            if dcycle_new != dcycle_cur {
                ipmi_lock.set_duty_cycle(*z, dcycle_new)?;
            }
        }

        Ok(())
    }

    /// Log a source that starts failing as an error and one that reads again
    /// as information, once per change
    fn log_transitions(session_name: &str, zone_config: &Zone, transitions: &[Transition]) {
        let describe = |index: usize| {
            zone_config.sources.get(index).map_or_else(
                || format!("#{}", index),
                |s| format!("{:?} (on_failure {:?})", s.kind, s.on_failure))
        };

        for transition in transitions {
            match transition {
                Transition::Failed { source, error } => {
                    error!("[{}] Zones {:?}: source {} failed: {}",
                           session_name, zone_config.fan_list, describe(*source), error);
                }
                Transition::Recovered { source } => {
                    info!("[{}] Zones {:?}: source {} reads again",
                          session_name, zone_config.fan_list, describe(*source));
                }
            }
        }
    }
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

    let mut app = MainApp::new(config)?;
    app.run().await
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
