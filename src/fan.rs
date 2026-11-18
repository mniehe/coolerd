use {
    std::panic::{catch_unwind, AssertUnwindSafe},
    log::error,
    crate::error::{Error, Result},
};

/// What one fan did with a duty cycle, in percent, for the debug log
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Applied {
    /// The duty cycle the fan had before, when the fan can read it
    pub before: Option<u8>,
    /// The duty cycle written, or `None` when the fan needed no write
    pub sent: Option<u8>,
}

/// One output that the daemon drives with a duty cycle
pub trait Fan: Send {
    /// A name for the fan in log lines and errors
    fn describe(&self) -> String;
    /// Take the fan over from whatever drove it before
    fn take_control(&mut self) -> Result<()>;
    /// Drive the fan at `duty` percent. The fan decides whether it writes.
    fn set_duty(&mut self, duty: u8) -> Result<Applied>;
    /// Leave the fan safe. Must be safe after a failed or partial
    /// `take_control`, and safe to call twice.
    fn release(&mut self) -> Result<()>;
}

/// Wrap an error of the fan described by `description`
fn fan_error(description: &str, source: Error) -> Error {
    Error::Fan { fan: description.to_owned(), source: Box::new(source) }
}

/// The fans of one zone. Dropping the guard releases every fan it holds, last
/// added first, also after an error or a panic.
pub struct FanGuard {
    /// Each fan with the description taken when it was added, so that `Drop`
    /// can name a fan without calling into it
    fans: Vec<(String, Box<dyn Fan>)>,
}

impl FanGuard {
    pub fn new() -> Self {
        Self { fans: Vec::new() }
    }

    /// Hold `fan` and take it over. The fan is held before the take-over, so
    /// a take-over that fails part way is still released.
    pub fn add(&mut self, fan: Box<dyn Fan>) -> Result<()> {
        self.fans.push((fan.describe(), fan));

        let index = self.fans.len() - 1;
        let (description, fan) = &mut self.fans[index];

        fan.take_control().map_err(|e| fan_error(description, e))
    }

    /// Drive every fan at `duty`, in the order they were added. Returns at
    /// the first error.
    pub fn set_duty(&mut self, duty: u8) -> Result<Vec<Applied>> {
        self.fans.iter_mut()
            .map(|(description, fan)| fan.set_duty(duty).map_err(|e| fan_error(description, e)))
            .collect()
    }

    /// The description of each fan, in the order they were added
    pub fn descriptions(&self) -> Vec<&str> {
        self.fans.iter().map(|(description, _)| description.as_str()).collect()
    }
}

impl Drop for FanGuard {
    fn drop(&mut self) {
        for (description, fan) in self.fans.iter_mut().rev() {
            match catch_unwind(AssertUnwindSafe(|| fan.release())) {
                Ok(Ok(())) => {}
                Ok(Err(e)) => error!("Failed to release {}: {}", description, e),
                Err(_) => error!("Release of {} panicked", description),
            }
        }
    }
}

#[cfg(test)]
pub mod fake {
    use {
        std::sync::{Arc, Mutex},
        super::{Applied, Fan},
        crate::error::{Error, Result},
    };

    /// An operation of the [`Fan`] trait
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum Op {
        TakeControl,
        SetDuty,
        Release,
    }

    /// One call to a fake fan: the fan's name and the operation
    pub type Call = (&'static str, Op);

    /// The calls of every fake fan that shares it, in order
    pub type CallLog = Arc<Mutex<Vec<Call>>>;

    /// A fan that records its calls in a shared log. One operation can fail
    /// and one can panic.
    pub struct FakeFan {
        pub name: &'static str,
        pub calls: CallLog,
        pub failing: Option<Op>,
        pub panicking: Option<Op>,
    }

    impl FakeFan {
        pub fn new(name: &'static str, calls: &CallLog) -> Self {
            Self { name, calls: calls.clone(), failing: None, panicking: None }
        }

        fn call(&self, op: Op) -> Result<()> {
            self.calls.lock().unwrap().push((self.name, op));

            if self.panicking == Some(op) {
                panic!("{} panics in {:?}", self.name, op);
            }

            if self.failing == Some(op) {
                return Err(Error::SensorNotFound(format!("{} fails {:?}", self.name, op)));
            }

            Ok(())
        }
    }

    impl Fan for FakeFan {
        fn describe(&self) -> String {
            self.name.to_owned()
        }

        fn take_control(&mut self) -> Result<()> {
            self.call(Op::TakeControl)
        }

        fn set_duty(&mut self, duty: u8) -> Result<Applied> {
            self.call(Op::SetDuty)?;
            Ok(Applied { before: None, sent: Some(duty) })
        }

        fn release(&mut self) -> Result<()> {
            self.call(Op::Release)
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::{
            fake::{CallLog, FakeFan, Op},
            Applied,
            FanGuard,
        },
        crate::error::{Error, Result},
    };

    fn log_of(calls: &CallLog) -> Vec<(&'static str, Op)> {
        calls.lock().unwrap().clone()
    }

    /// Add `fans` in order as the zone loop does, returning at the first error
    fn guard_over(fans: Vec<FakeFan>) -> Result<FanGuard> {
        let mut guard = FanGuard::new();

        for fan in fans {
            guard.add(Box::new(fan))?;
        }

        Ok(guard)
    }

    /// The description of the fan that a [`Error::Fan`] names
    fn failed_fan(error: Error) -> String {
        match error {
            Error::Fan { fan, .. } => fan,
            other => panic!("expected a fan error, got {:?}", other),
        }
    }

    #[test]
    fn drop_releases_every_fan_after_a_failed_release() {
        let calls = CallLog::default();
        let second = FakeFan { failing: Some(Op::Release), ..FakeFan::new("b", &calls) };
        let guard = guard_over(vec![FakeFan::new("a", &calls), second, FakeFan::new("c", &calls)]).unwrap();

        drop(guard);

        assert_eq!(log_of(&calls), [
            ("a", Op::TakeControl), ("b", Op::TakeControl), ("c", Op::TakeControl),
            ("c", Op::Release), ("b", Op::Release), ("a", Op::Release),
        ]);
    }

    #[test]
    fn drop_releases_every_fan_after_a_panicking_release() {
        let calls = CallLog::default();
        let second = FakeFan { panicking: Some(Op::Release), ..FakeFan::new("b", &calls) };
        let guard = guard_over(vec![FakeFan::new("a", &calls), second, FakeFan::new("c", &calls)]).unwrap();

        drop(guard);

        assert_eq!(log_of(&calls)[3..], [("c", Op::Release), ("b", Op::Release), ("a", Op::Release)]);
    }

    #[test]
    fn failed_take_control_releases_the_fans_added_so_far() {
        let calls = CallLog::default();
        let second = FakeFan { failing: Some(Op::TakeControl), ..FakeFan::new("b", &calls) };

        let error = guard_over(vec![FakeFan::new("a", &calls), second, FakeFan::new("c", &calls)])
            .err().expect("the second take-over fails");

        assert_eq!(failed_fan(error), "b");
        assert_eq!(log_of(&calls), [
            ("a", Op::TakeControl), ("b", Op::TakeControl),
            ("b", Op::Release), ("a", Op::Release),
        ]);
    }

    #[test]
    fn set_duty_stops_at_the_first_failing_fan() {
        let calls = CallLog::default();
        let first = FakeFan { failing: Some(Op::SetDuty), ..FakeFan::new("a", &calls) };
        let mut guard = guard_over(vec![first, FakeFan::new("b", &calls)]).unwrap();

        let error = guard.set_duty(60).err().expect("the first fan fails");
        let calls_before_drop = log_of(&calls);
        drop(guard);

        assert_eq!(failed_fan(error), "a");
        assert_eq!(calls_before_drop, [("a", Op::TakeControl), ("b", Op::TakeControl), ("a", Op::SetDuty)]);
    }

    #[test]
    fn set_duty_reports_each_fan_in_config_order() {
        let calls = CallLog::default();
        let mut guard = guard_over(vec![FakeFan::new("a", &calls), FakeFan::new("b", &calls)]).unwrap();

        let applied = guard.set_duty(60).unwrap();

        assert_eq!(applied, [Applied { before: None, sent: Some(60) }; 2]);
        assert_eq!(log_of(&calls)[2..], [("a", Op::SetDuty), ("b", Op::SetDuty)]);
    }
}
