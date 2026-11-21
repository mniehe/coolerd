use {
    std::{
        collections::HashMap,
        sync::{
            Arc,
            Mutex,
            MutexGuard,
            PoisonError,
            atomic::{AtomicBool, Ordering},
        },
    },
    log::{error, info},
    crate::{
        config::SessionType,
        error::Result,
        freeipmi::SensorReading,
        ipmi::{FanMode, Ipmi},
    },
};

/// The board operations of an IPMI session, so that a test can fake the board
pub trait IpmiPort: Send {
    fn get_fan_mode(&mut self) -> Result<FanMode>;
    fn set_fan_mode(&mut self, mode: FanMode) -> Result<()>;
    fn get_duty_cycle(&mut self, zone: u8) -> Result<u8>;
    fn set_duty_cycle(&mut self, zone: u8, dcycle: u8) -> Result<()>;
    fn get_temperature_readings(&mut self) -> Result<HashMap<String, Option<SensorReading>>>;
}

/// Lock `mutex`, also when a thread panicked while holding it. A panic in one
/// zone task must not stop the shutdown path from setting the fans to 100%.
fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether every session set its zones to 100% and restored the fan mode it
/// saved
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestoreOutcome {
    AllRestored,
    SomeFailed,
}

/// The record of failed restores that every session shares, so that a failed
/// write of 100% or of the saved fan mode can reach the exit status after the
/// sessions drop
#[derive(Clone, Debug, Default)]
pub struct RestoreFailures(Arc<AtomicBool>);

impl RestoreFailures {
    fn record(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn outcome(&self) -> RestoreOutcome {
        if self.0.load(Ordering::SeqCst) {
            RestoreOutcome::SomeFailed
        } else {
            RestoreOutcome::AllRestored
        }
    }
}

/// One IPMI connection, shared by the zones that use it. All communication
/// with the board is behind a mutex to avoid needing multiple IPMI sessions.
pub struct IpmiSession {
    /// Session name (for logging only)
    name: String,
    /// The board
    port: Mutex<Box<dyn IpmiPort>>,
    /// Original fan mode
    orig_fan_mode: FanMode,
    /// Set these zones to dcycle 100% before restoring original fan mode
    restore_zones: Vec<u8>,
    /// Where `Drop` records a failed write of 100% or of the original fan mode
    restore_failures: RestoreFailures,
}

impl IpmiSession {
    pub fn new<N, R>(name: N, st: &SessionType, restore_zones: R, restore_failures: RestoreFailures) -> Result<Self>
    where
        N: AsRef<str>,
        R: IntoIterator<Item = u8>,
    {
        Self::with_port(name, Box::new(Ipmi::new(st)?), restore_zones, restore_failures)
    }

    /// Take over the board behind `port`. The session exists before the
    /// switch to full fan mode, so a failed switch drops it, and its `Drop`
    /// restores the saved mode.
    pub fn with_port<N, R>(name: N, mut port: Box<dyn IpmiPort>, restore_zones: R, restore_failures: RestoreFailures) -> Result<Self>
    where
        N: AsRef<str>,
        R: IntoIterator<Item = u8>,
    {
        let orig_fan_mode = port.get_fan_mode()?;

        info!("[{}] Original fan mode: {:?}", name.as_ref(), orig_fan_mode);

        let session = Self {
            name: name.as_ref().to_owned(),
            port: Mutex::new(port),
            orig_fan_mode,
            restore_zones: restore_zones.into_iter().collect(),
            restore_failures,
        };

        if orig_fan_mode != FanMode::Full {
            info!("[{}] Setting fan mode to: {:?}", session.name, FanMode::Full);
            // Bound first, so the lock is released before `Drop` takes it
            let switched = session.lock().set_fan_mode(FanMode::Full);
            switched?;
        }

        Ok(session)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Lock the board for a sequence of calls
    pub fn lock(&self) -> MutexGuard<'_, Box<dyn IpmiPort>> {
        lock_or_recover(&self.port)
    }
}

impl Drop for IpmiSession {
    fn drop(&mut self) {
        let mut port = lock_or_recover(&self.port);

        // A signal can cancel a zone task before its fan guard exists, so
        // this write can be the only one of 100% that the zone gets
        for z in &self.restore_zones {
            info!("[{}] Setting zone {} duty cycle to 100%", self.name, z);
            if let Err(e) = port.set_duty_cycle(*z, 100) {
                error!("[{}] Failed to set zone {} duty cycle to 100%: {}", self.name, z, e);
                self.restore_failures.record();
            }
        }

        if self.orig_fan_mode != FanMode::Full {
            info!("[{}] Restoring fan mode to: {:?}", self.name, self.orig_fan_mode);
            if let Err(e) = port.set_fan_mode(self.orig_fan_mode) {
                error!("[{}] Failed to restore fan mode: {}", self.name, e);
                self.restore_failures.record();
            }
        }
    }
}

#[cfg(test)]
pub mod fake {
    use {
        std::{
            collections::HashMap,
            sync::{Arc, Mutex},
        },
        super::IpmiPort,
        crate::{
            error::{Error, Result},
            freeipmi::SensorReading,
            ipmi::{self, FanMode},
        },
    };

    /// One call to a fake board
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum PortCall {
        GetFanMode,
        SetFanMode(FanMode),
        GetDutyCycle(u8),
        SetDutyCycle(u8, u8),
        GetTemperatureReadings,
    }

    /// The calls made on a fake board, in order
    pub type BoardLog = Arc<Mutex<Vec<PortCall>>>;

    /// A board that records every call. Its duty cycles start at 0 unless set
    /// in `duties`, and one call can fail.
    pub struct FakeBoard {
        pub calls: BoardLog,
        pub fan_mode: FanMode,
        pub duties: HashMap<u8, u8>,
        pub failing: Option<PortCall>,
    }

    impl FakeBoard {
        pub fn new(fan_mode: FanMode, calls: &BoardLog) -> Self {
            Self { calls: calls.clone(), fan_mode, duties: HashMap::new(), failing: None }
        }

        fn record(&self, call: PortCall) -> Result<()> {
            self.calls.lock().unwrap().push(call);

            if self.failing == Some(call) {
                return Err(Error::Ipmi(ipmi::Error::BadResponseSize { expected: 0, actual: 1 }));
            }

            Ok(())
        }
    }

    impl IpmiPort for FakeBoard {
        fn get_fan_mode(&mut self) -> Result<FanMode> {
            self.record(PortCall::GetFanMode)?;
            Ok(self.fan_mode)
        }

        fn set_fan_mode(&mut self, mode: FanMode) -> Result<()> {
            self.record(PortCall::SetFanMode(mode))?;
            self.fan_mode = mode;
            Ok(())
        }

        fn get_duty_cycle(&mut self, zone: u8) -> Result<u8> {
            self.record(PortCall::GetDutyCycle(zone))?;
            Ok(self.duties.get(&zone).copied().unwrap_or(0))
        }

        fn set_duty_cycle(&mut self, zone: u8, dcycle: u8) -> Result<()> {
            self.record(PortCall::SetDutyCycle(zone, dcycle))?;
            self.duties.insert(zone, dcycle);
            Ok(())
        }

        fn get_temperature_readings(&mut self) -> Result<HashMap<String, Option<SensorReading>>> {
            self.record(PortCall::GetTemperatureReadings)?;
            Ok(HashMap::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        std::{
            sync::{Arc, Mutex},
            thread,
        },
        super::{
            fake::{BoardLog, FakeBoard, PortCall},
            lock_or_recover,
            IpmiSession,
            RestoreFailures,
            RestoreOutcome,
        },
        crate::{
            error::Error,
            ipmi::FanMode,
        },
    };

    fn log_of(calls: &BoardLog) -> Vec<PortCall> {
        calls.lock().unwrap().clone()
    }

    #[test]
    fn lock_recovers_a_poisoned_mutex() {
        let mutex = Arc::new(Mutex::new(1u8));
        let holder = mutex.clone();

        let panicked = thread::spawn(move || {
            let _guard = holder.lock().unwrap();
            panic!("panic while holding the lock");
        }).join();

        assert!(panicked.is_err());
        assert!(mutex.is_poisoned());

        let mut guard = lock_or_recover(&mutex);
        *guard += 1;
        assert_eq!(*guard, 2);
    }

    #[test]
    fn failed_switch_to_full_mode_restores_the_saved_mode() {
        let calls = BoardLog::default();
        let board = FakeBoard {
            failing: Some(PortCall::SetFanMode(FanMode::Full)),
            ..FakeBoard::new(FanMode::Optimal, &calls)
        };

        let result = IpmiSession::with_port("test", Box::new(board), [0], RestoreFailures::default());

        assert!(matches!(result, Err(Error::Ipmi(_))), "expected the switch error");
        assert_eq!(log_of(&calls), [
            PortCall::GetFanMode,
            PortCall::SetFanMode(FanMode::Full),
            PortCall::SetDutyCycle(0, 100),
            PortCall::SetFanMode(FanMode::Optimal),
        ]);
    }

    #[test]
    fn drop_sets_zones_to_full_duty_before_restoring_the_mode() {
        let calls = BoardLog::default();
        let session = IpmiSession::with_port("test", Box::new(FakeBoard::new(FanMode::Standard, &calls)), [0, 1], RestoreFailures::default()).unwrap();

        drop(session);

        assert_eq!(log_of(&calls), [
            PortCall::GetFanMode,
            PortCall::SetFanMode(FanMode::Full),
            PortCall::SetDutyCycle(0, 100),
            PortCall::SetDutyCycle(1, 100),
            PortCall::SetFanMode(FanMode::Standard),
        ]);
    }

    #[test]
    fn drop_restores_the_board_after_a_panic_poisoned_the_port() {
        let calls = BoardLog::default();
        let session = IpmiSession::with_port("test", Box::new(FakeBoard::new(FanMode::Standard, &calls)), [0], RestoreFailures::default()).unwrap();

        let panicked = thread::scope(|scope| {
            scope.spawn(|| {
                let _port = session.lock();
                panic!("panic while holding the port");
            }).join()
        });

        assert!(panicked.is_err());
        assert!(session.port.is_poisoned());

        drop(session);

        assert!(log_of(&calls).ends_with(&[
            PortCall::SetDutyCycle(0, 100),
            PortCall::SetFanMode(FanMode::Standard),
        ]), "{:?}", log_of(&calls));
    }

    #[test]
    fn drop_sets_later_zones_and_the_mode_after_a_failed_zone_write() {
        let calls = BoardLog::default();
        let board = FakeBoard {
            failing: Some(PortCall::SetDutyCycle(0, 100)),
            ..FakeBoard::new(FanMode::Standard, &calls)
        };
        let failures = RestoreFailures::default();
        let session = IpmiSession::with_port("test", Box::new(board), [0, 1], failures.clone()).unwrap();

        drop(session);

        assert_eq!(log_of(&calls), [
            PortCall::GetFanMode,
            PortCall::SetFanMode(FanMode::Full),
            PortCall::SetDutyCycle(0, 100),
            PortCall::SetDutyCycle(1, 100),
            PortCall::SetFanMode(FanMode::Standard),
        ]);
        assert_eq!(failures.outcome(), RestoreOutcome::SomeFailed);
    }

    #[test]
    fn failed_mode_restore_is_recorded() {
        let calls = BoardLog::default();
        let board = FakeBoard {
            failing: Some(PortCall::SetFanMode(FanMode::Standard)),
            ..FakeBoard::new(FanMode::Standard, &calls)
        };
        let failures = RestoreFailures::default();
        let session = IpmiSession::with_port("test", Box::new(board), [0], failures.clone()).unwrap();

        drop(session);

        assert_eq!(log_of(&calls), [
            PortCall::GetFanMode,
            PortCall::SetFanMode(FanMode::Full),
            PortCall::SetDutyCycle(0, 100),
            PortCall::SetFanMode(FanMode::Standard),
        ]);
        assert_eq!(failures.outcome(), RestoreOutcome::SomeFailed);
    }

    #[test]
    fn restored_mode_records_no_failure() {
        let calls = BoardLog::default();
        let failures = RestoreFailures::default();
        let session = IpmiSession::with_port("test", Box::new(FakeBoard::new(FanMode::Standard, &calls)), [0], failures.clone()).unwrap();

        drop(session);

        assert_eq!(failures.outcome(), RestoreOutcome::AllRestored);
    }

    #[test]
    fn board_in_full_mode_gets_no_mode_write() {
        let calls = BoardLog::default();
        let session = IpmiSession::with_port("test", Box::new(FakeBoard::new(FanMode::Full, &calls)), [0], RestoreFailures::default()).unwrap();

        drop(session);

        assert_eq!(log_of(&calls), [PortCall::GetFanMode, PortCall::SetDutyCycle(0, 100)]);
    }
}
