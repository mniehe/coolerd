use {
    std::{
        fs,
        panic::{catch_unwind, AssertUnwindSafe},
        path::{Path, PathBuf},
        sync::Arc,
    },
    log::{error, info},
    crate::{
        config::FanConfig,
        error::{Error, Result},
        hwmon::{HwmonRoot, read_number, write_number},
        nvml::Nvml,
        session::IpmiSession,
    },
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

/// The duty cycle that leaves a Supermicro zone safe
const SUPERMICRO_RELEASE_DUTY: u8 = 100;

/// One Supermicro IPMI fan zone on a shared session. The session switches the
/// board to full fan mode when it opens and restores the saved mode when it
/// drops, after the guards that hold its zones.
pub struct SupermicroZone {
    session: Arc<IpmiSession>,
    zone: u8,
}

impl SupermicroZone {
    pub fn new(session: Arc<IpmiSession>, zone: u8) -> Self {
        Self { session, zone }
    }
}

impl Fan for SupermicroZone {
    fn describe(&self) -> String {
        format!("supermicro zone {} on session {}", self.zone, self.session.name())
    }

    fn take_control(&mut self) -> Result<()> {
        Ok(())
    }

    fn set_duty(&mut self, duty: u8) -> Result<Applied> {
        let mut port = self.session.lock();
        let before = port.get_duty_cycle(self.zone)?;

        if before == duty {
            return Ok(Applied { before: Some(before), sent: None });
        }

        port.set_duty_cycle(self.zone, duty)?;

        Ok(Applied { before: Some(before), sent: Some(duty) })
    }

    fn release(&mut self) -> Result<()> {
        info!("[{}] Setting zone {} duty cycle to {}%", self.session.name(), self.zone, SUPERMICRO_RELEASE_DUTY);
        self.session.lock().set_duty_cycle(self.zone, SUPERMICRO_RELEASE_DUTY)
    }
}

/// The `pwm<N>_enable` value that puts a channel under manual control, from
/// the kernel's hwmon sysfs interface (Documentation/hwmon/sysfs-interface.rst)
const PWM_ENABLE_MANUAL: u32 = 1;

/// The lowest saved `pwm<N>_enable` value that release writes back. Values
/// from 2 up are modes the driver defines, automatic on most chips. A saved 0
/// or 1 is never written back and the channel stays manual at full speed: 1
/// is manual already, and while the generic hwmon ABI reads 0 as full speed,
/// the kernel's pwm-fan driver reads it as "fan off".
const PWM_ENABLE_AUTOMATIC_MIN: u32 = 2;

/// The highest `pwm<N>` value, which is full speed
const PWM_FULL: u32 = 255;

/// The channel state saved at take-over
struct Taken {
    /// The chip's canonical directory, resolved once at take-over
    device: PathBuf,
    /// `pwm<N>_enable` as it was before the take-over
    saved_enable: u32,
}

/// One PWM channel of a hwmon chip, found by the text of the chip's `name`
/// file. Take-over saves the channel's mode and switches it to manual; release
/// runs it at full speed and gives the saved mode back.
pub struct HwmonPwm {
    root: HwmonRoot,
    chip: String,
    pwm: u8,
    taken: Option<Taken>,
}

impl HwmonPwm {
    pub fn new(root: HwmonRoot, chip: String, pwm: u8) -> Self {
        Self { root, chip, pwm, taken: None }
    }

    fn pwm_path(&self, device: &Path) -> PathBuf {
        device.join(format!("pwm{}", self.pwm))
    }

    fn enable_path(&self, device: &Path) -> PathBuf {
        device.join(format!("pwm{}_enable", self.pwm))
    }
}

/// Read an attribute that must hold a number from 0 to `max`
fn read_unsigned(path: &Path, max: u32) -> Result<u32> {
    let value = read_number(path)?;

    u32::try_from(value).ok()
        .filter(|raw| *raw <= max)
        .ok_or_else(|| Error::HwmonValueOutOfRange { path: path.to_owned(), value })
}

/// The raw PWM value of `duty` percent, rounded up so the fan never runs
/// slower than asked
fn duty_to_raw(duty: u8) -> u32 {
    (u32::from(duty) * PWM_FULL).div_ceil(100).min(PWM_FULL)
}

/// The percentage of a raw PWM value from 0 to [`PWM_FULL`], rounded up
fn raw_to_percent(raw: u32) -> u8 {
    u8::try_from((raw * 100).div_ceil(PWM_FULL)).unwrap_or(u8::MAX)
}

impl Fan for HwmonPwm {
    fn describe(&self) -> String {
        format!("hwmon {} pwm{}", self.chip, self.pwm)
    }

    /// Resolve the chip once, save the channel's mode, then switch it to
    /// manual at full speed. The mode is saved before any write, so a release
    /// after a failed write gives it back.
    fn take_control(&mut self) -> Result<()> {
        if self.taken.is_some() {
            return Ok(());
        }

        let resolved = self.root.resolve(&self.chip)?;
        let device = fs::canonicalize(&resolved)
            .map_err(|e| Error::Io { path: resolved, source: e })?;
        let enable_path = self.enable_path(&device);
        let pwm_path = self.pwm_path(&device);
        let saved_enable = read_unsigned(&enable_path, u32::MAX)?;

        info!("[{}] Taking over {:?}, saved pwm{}_enable {}", self.chip, device, self.pwm, saved_enable);
        self.taken = Some(Taken { device, saved_enable });

        write_number(&enable_path, PWM_ENABLE_MANUAL)?;
        write_number(&pwm_path, PWM_FULL)
    }

    fn set_duty(&mut self, duty: u8) -> Result<Applied> {
        let taken = self.taken.as_ref().ok_or_else(|| Error::FanNotTaken(self.describe()))?;
        let path = self.pwm_path(&taken.device);
        let target = duty_to_raw(duty);
        let raw = read_unsigned(&path, PWM_FULL)?;
        let before = Some(raw_to_percent(raw));

        if raw == target {
            return Ok(Applied { before, sent: None });
        }

        write_number(&path, target)?;

        Ok(Applied { before, sent: Some(raw_to_percent(target)) })
    }

    /// Run the channel at full speed while it is still manual, then write
    /// back a saved mode of [`PWM_ENABLE_AUTOMATIC_MIN`] or more. A saved 0
    /// or 1 leaves the channel manual at full speed. Both writes are tried;
    /// the first error is returned and a second one is logged.
    fn release(&mut self) -> Result<()> {
        let Some(taken) = self.taken.take() else {
            return Ok(());
        };

        let restores = taken.saved_enable >= PWM_ENABLE_AUTOMATIC_MIN;

        if restores {
            info!("[{}] Setting pwm{} to {} and restoring pwm{}_enable {}",
                  self.chip, self.pwm, PWM_FULL, self.pwm, taken.saved_enable);
        } else {
            info!("[{}] Setting pwm{} to {} and leaving it manual; saved pwm{}_enable {} is not automatic",
                  self.chip, self.pwm, PWM_FULL, self.pwm, taken.saved_enable);
        }

        let full = write_number(&self.pwm_path(&taken.device), PWM_FULL);
        let restore = if restores {
            write_number(&self.enable_path(&taken.device), taken.saved_enable)
        } else {
            Ok(())
        };

        match (full, restore) {
            (Err(first), Err(second)) => {
                error!("Failed to restore pwm{}_enable of {}: {}", self.pwm, self.chip, second);
                Err(first)
            }
            (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e),
            (Ok(()), Ok(())) => Ok(()),
        }
    }
}

/// The highest fan speed NVML can report as a limit, in percent
const NVIDIA_SPEED_MAX: u32 = 100;

/// One fan of an NVIDIA GPU, found by the GPU's PCI bus id and driven through
/// NVML. Take-over only reads the fan; the first duty switches it to manual
/// control, and release gives it back to the driver's default policy.
pub struct NvidiaFan {
    nvml: Arc<dyn Nvml>,
    gpu: String,
    fan: u32,
    /// The (min, max) speed the fan accepts, read at take-over. `None` until a
    /// take-over succeeds and again after release.
    limits: Option<(u32, u32)>,
    /// The speed sent last since take-over
    last_sent: Option<u32>,
}

impl NvidiaFan {
    pub fn new(nvml: Arc<dyn Nvml>, gpu: String, fan: u32) -> Self {
        Self { nvml, gpu, fan, limits: None, last_sent: None }
    }

    /// The fan's limits, which must be a range within 0 to 100 percent
    fn read_limits(&self) -> Result<(u32, u32)> {
        let (min, max) = self.nvml.fan_limits(&self.gpu)?;

        if min > max || max > NVIDIA_SPEED_MAX {
            return Err(Error::NvidiaFanLimits { gpu: self.gpu.clone(), min, max });
        }

        Ok((min, max))
    }
}

/// The speed NVML is sent for `duty`: raised to the fan's minimum, because
/// NVML rejects a lower speed, and capped at its maximum
fn nvidia_target(duty: u8, (min, max): (u32, u32)) -> u32 {
    u32::from(duty).max(min).min(max)
}

impl Fan for NvidiaFan {
    fn describe(&self) -> String {
        format!("nvidia gpu {} fan {}", self.gpu, self.fan)
    }

    /// Check that the fan exists and keep its limits. Nothing is written, so
    /// a failed take-over leaves the fan under the driver's control.
    fn take_control(&mut self) -> Result<()> {
        let count = self.nvml.fan_count(&self.gpu)?;

        if self.fan >= count {
            return Err(Error::NvidiaFanMissing { gpu: self.gpu.clone(), fan: self.fan, count });
        }

        let (min, max) = self.read_limits()?;

        info!("[{}] Taking over fan {}, speed limits {}% to {}%", self.gpu, self.fan, min, max);
        self.limits = Some((min, max));
        self.last_sent = None;

        Ok(())
    }

    /// Send the target speed on the first call after take-over, whatever the
    /// fan reports, because the reported speed does not show whether the
    /// driver or this program controls the fan. Later calls send only a
    /// changed target.
    fn set_duty(&mut self, duty: u8) -> Result<Applied> {
        let limits = self.limits.ok_or_else(|| Error::FanNotTaken(self.describe()))?;
        let reported = self.nvml.fan_speed(&self.gpu, self.fan)?;
        let before = Some(u8::try_from(reported).unwrap_or(u8::MAX));
        let target = nvidia_target(duty, limits);

        if self.last_sent == Some(target) {
            return Ok(Applied { before, sent: None });
        }

        self.nvml.set_fan_speed(&self.gpu, self.fan, target)?;
        self.last_sent = Some(target);

        // The target is at most NVIDIA_SPEED_MAX, so it fits
        Ok(Applied { before, sent: Some(u8::try_from(target).unwrap_or(u8::MAX)) })
    }

    /// Ask for the default fan policy. When that fails, run the fan at its
    /// maximum under manual control, and return the first error.
    fn release(&mut self) -> Result<()> {
        let Some((_, max)) = self.limits.take() else {
            return Ok(());
        };

        self.last_sent = None;
        info!("[{}] Giving fan {} back to the default fan policy", self.gpu, self.fan);

        let Err(default) = self.nvml.set_default_fan_speed(&self.gpu, self.fan) else {
            return Ok(());
        };

        error!("Failed to give fan {} of GPU {} back to the default policy: {}; setting it to {}%",
               self.fan, self.gpu, default, max);

        if let Err(full) = self.nvml.set_fan_speed(&self.gpu, self.fan, max) {
            error!("Failed to set fan {} of GPU {} to {}%: {}", self.fan, self.gpu, max, full);
        }

        Err(default)
    }
}

/// Build the fan that `config` names. This is the only place that turns a
/// fan config into a fan. A Supermicro fan needs the zone's IPMI session; a
/// hwmon fan finds its chip under `hwmon`; an NVIDIA fan calls `nvml`.
pub fn build_fan(config: &FanConfig, session: Option<&Arc<IpmiSession>>, hwmon: &HwmonRoot, nvml: &Arc<dyn Nvml>) -> Result<Box<dyn Fan>> {
    match config {
        FanConfig::Supermicro { zone } => {
            let session = session
                .ok_or_else(|| Error::FanWithoutSession(format!("supermicro zone {}", zone)))?;

            Ok(Box::new(SupermicroZone::new(session.clone(), *zone)))
        }
        FanConfig::Hwmon { chip, pwm } => Ok(Box::new(HwmonPwm::new(hwmon.clone(), chip.clone(), *pwm))),
        FanConfig::Nvidia { gpu, fan } => Ok(Box::new(NvidiaFan::new(nvml.clone(), gpu.clone(), *fan))),
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
        std::{
            fs,
            os::unix::fs::symlink,
            path::PathBuf,
            sync::Arc,
            time::{Duration, Instant},
        },
        super::{
            build_fan,
            fake::{CallLog, FakeFan, Op},
            Applied,
            Fan,
            FanGuard,
            HwmonPwm,
            NvidiaFan,
            SupermicroZone,
        },
        crate::{
            command::testing::TempDir,
            config::FanConfig,
            error::{Error, Result},
            hwmon::HwmonRoot,
            ipmi::FanMode,
            nvml::{
                fake::{stalling_worker, Call, FakeGpu, FakeNvml, Op as NvmlOp},
                Limits,
                Nvml,
            },
            session::{
                fake::{BoardLog, FakeBoard, PortCall},
                IpmiSession,
            },
        },
    };

    /// An NVML with no GPU, for fans that never call it
    fn no_nvml() -> Arc<dyn Nvml> {
        Arc::new(FakeNvml::default())
    }

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

    /// The calls a session makes when it opens on a board in standard mode
    const SESSION_OPEN: [PortCall; 2] = [PortCall::GetFanMode, PortCall::SetFanMode(FanMode::Full)];

    fn session_on(board: FakeBoard) -> Arc<IpmiSession> {
        Arc::new(IpmiSession::with_port("test", Box::new(board), [0]).unwrap())
    }

    #[test]
    fn supermicro_zone_writes_only_a_changed_duty() {
        let calls = BoardLog::default();
        let mut board = FakeBoard::new(FanMode::Standard, &calls);
        board.duties.insert(0, 40);
        let mut zone = SupermicroZone::new(session_on(board), 0);

        let changed = zone.set_duty(60).unwrap();
        let unchanged = zone.set_duty(60).unwrap();

        assert_eq!(changed, Applied { before: Some(40), sent: Some(60) });
        assert_eq!(unchanged, Applied { before: Some(60), sent: None });
        assert_eq!(calls.lock().unwrap()[SESSION_OPEN.len()..], [
            PortCall::GetDutyCycle(0),
            PortCall::SetDutyCycle(0, 60),
            PortCall::GetDutyCycle(0),
        ]);
    }

    #[test]
    fn supermicro_zone_reports_a_failed_write() {
        let calls = BoardLog::default();
        let board = FakeBoard { failing: Some(PortCall::SetDutyCycle(0, 60)), ..FakeBoard::new(FanMode::Standard, &calls) };
        let mut zone = SupermicroZone::new(session_on(board), 0);

        let result = zone.set_duty(60);

        assert!(matches!(result, Err(Error::Ipmi(_))), "{:?}", result);
    }

    #[test]
    fn supermicro_zone_writes_nothing_after_a_failed_read() {
        let calls = BoardLog::default();
        let board = FakeBoard { failing: Some(PortCall::GetDutyCycle(0)), ..FakeBoard::new(FanMode::Standard, &calls) };
        let mut zone = SupermicroZone::new(session_on(board), 0);

        let result = zone.set_duty(60);

        assert!(matches!(result, Err(Error::Ipmi(_))), "{:?}", result);
        assert_eq!(calls.lock().unwrap()[SESSION_OPEN.len()..], [PortCall::GetDutyCycle(0)]);
    }

    #[test]
    fn supermicro_fan_needs_a_session() {
        let config = FanConfig::Supermicro { zone: 1 };

        let error = build_fan(&config, None, &HwmonRoot::default(), &no_nvml()).err().expect("a supermicro fan without a session fails");

        assert!(matches!(&error, Error::FanWithoutSession(fan) if fan == "supermicro zone 1"), "{:?}", error);
    }

    #[test]
    fn supermicro_fan_is_built_on_its_session() {
        let calls = BoardLog::default();
        let session = session_on(FakeBoard::new(FanMode::Standard, &calls));

        let fan = build_fan(&FanConfig::Supermicro { zone: 1 }, Some(&session), &HwmonRoot::default(), &no_nvml()).unwrap();

        assert_eq!(fan.describe(), "supermicro zone 1 on session test");
    }

    #[test]
    fn released_zone_runs_full_before_the_session_restores_the_mode() {
        let calls = BoardLog::default();

        {
            let session = session_on(FakeBoard::new(FanMode::Standard, &calls));
            let mut guard = FanGuard::new();
            guard.add(Box::new(SupermicroZone::new(session.clone(), 0))).unwrap();
        }

        assert_eq!(calls.lock().unwrap()[SESSION_OPEN.len()..], [
            // The guard's release
            PortCall::SetDutyCycle(0, 100),
            // The session's own drop
            PortCall::SetDutyCycle(0, 100),
            PortCall::SetFanMode(FanMode::Standard),
        ]);
    }

    /// A hwmon tree with chip `nvme` at `hwmon0` and chip `nct6798` at
    /// `hwmon1`, whose `pwm2_enable` holds `enable` and `pwm2` holds `pwm`
    struct Nct6798 {
        dir: TempDir,
    }

    impl Nct6798 {
        fn new(enable: &str, pwm: &str) -> Self {
            let dir = TempDir::new();

            for (entry, name) in [("hwmon0", "nvme"), ("hwmon1", "nct6798")] {
                fs::create_dir(dir.0.join(entry)).unwrap();
                fs::write(dir.0.join(entry).join("name"), format!("{}\n", name)).unwrap();
            }

            fs::write(dir.0.join("hwmon1/pwm2_enable"), enable).unwrap();
            fs::write(dir.0.join("hwmon1/pwm2"), pwm).unwrap();

            Self { dir }
        }

        fn path(&self, attribute: &str) -> PathBuf {
            self.dir.0.join("hwmon1").join(attribute)
        }

        fn read(&self, attribute: &str) -> String {
            fs::read_to_string(self.path(attribute)).unwrap()
        }

        fn write(&self, attribute: &str, value: &str) {
            fs::write(self.path(attribute), value).unwrap();
        }

        fn fan(&self) -> HwmonPwm {
            HwmonPwm::new(HwmonRoot::new(self.dir.0.clone()), "nct6798".into(), 2)
        }
    }

    #[test]
    fn hwmon_take_control_saves_the_mode_and_runs_full() {
        let chip = Nct6798::new("5\n", "127\n");
        let mut fan = chip.fan();

        fan.take_control().unwrap();

        assert_eq!(chip.read("pwm2_enable"), "1");
        assert_eq!(chip.read("pwm2"), "255");
    }

    #[test]
    fn hwmon_release_restores_the_saved_mode() {
        let chip = Nct6798::new("5\n", "127\n");
        let mut fan = chip.fan();
        fan.take_control().unwrap();
        fan.set_duty(20).unwrap();

        fan.release().unwrap();

        assert_eq!(chip.read("pwm2"), "255");
        assert_eq!(chip.read("pwm2_enable"), "5");
    }

    #[test]
    fn hwmon_release_restores_mode_2() {
        let chip = Nct6798::new("2\n", "127\n");
        let mut fan = chip.fan();
        fan.take_control().unwrap();
        fan.set_duty(20).unwrap();

        fan.release().unwrap();

        assert_eq!(chip.read("pwm2_enable"), "2");
        assert_eq!(chip.read("pwm2"), "255");
    }

    #[test]
    fn hwmon_set_duty_writes_only_a_changed_raw_value() {
        let chip = Nct6798::new("5\n", "127\n");
        let mut fan = chip.fan();
        fan.take_control().unwrap();
        chip.write("pwm2", "127\n");

        let changed = fan.set_duty(50).unwrap();
        assert_eq!(chip.read("pwm2"), "128");
        // A write would replace the line ending as well
        chip.write("pwm2", "128\n");
        let unchanged = fan.set_duty(50).unwrap();

        assert_eq!(changed, Applied { before: Some(50), sent: Some(51) });
        assert_eq!(unchanged, Applied { before: Some(51), sent: None });
        assert_eq!(chip.read("pwm2"), "128\n");
    }

    #[test]
    fn hwmon_set_duty_scales_the_ends_of_the_range() {
        let chip = Nct6798::new("5\n", "127\n");
        let mut fan = chip.fan();
        fan.take_control().unwrap();

        fan.set_duty(0).unwrap();
        assert_eq!(chip.read("pwm2"), "0");

        fan.set_duty(100).unwrap();
        assert_eq!(chip.read("pwm2"), "255");
    }

    #[test]
    fn hwmon_release_from_manual_mode_leaves_full_speed() {
        let chip = Nct6798::new("1\n", "90\n");
        let mut fan = chip.fan();
        fan.take_control().unwrap();
        fan.set_duty(20).unwrap();

        fan.release().unwrap();

        assert_eq!(chip.read("pwm2"), "255");
        assert_eq!(chip.read("pwm2_enable"), "1");
    }

    #[test]
    fn hwmon_release_from_mode_0_leaves_manual_full_speed() {
        let chip = Nct6798::new("0\n", "90\n");
        let mut fan = chip.fan();
        fan.take_control().unwrap();
        fan.set_duty(20).unwrap();

        fan.release().unwrap();

        assert_eq!(chip.read("pwm2"), "255");
        assert_eq!(chip.read("pwm2_enable"), "1");
    }

    /// Replace `attribute` with a directory, so that a write to it fails for
    /// any user
    fn break_attribute(chip: &Nct6798, attribute: &str) {
        fs::remove_file(chip.path(attribute)).unwrap();
        fs::create_dir(chip.path(attribute)).unwrap();
    }

    #[test]
    fn hwmon_release_runs_full_when_the_mode_cannot_be_restored() {
        let chip = Nct6798::new("5\n", "127\n");
        let mut fan = chip.fan();
        fan.take_control().unwrap();
        fan.set_duty(20).unwrap();
        break_attribute(&chip, "pwm2_enable");

        let result = fan.release();

        assert!(matches!(&result, Err(Error::Io { path, .. }) if *path == chip.path("pwm2_enable")), "{:?}", result);
        assert_eq!(chip.read("pwm2"), "255");
    }

    #[test]
    fn hwmon_release_restores_the_mode_after_a_failed_full_write() {
        let chip = Nct6798::new("5\n", "127\n");
        let mut fan = chip.fan();
        fan.take_control().unwrap();
        break_attribute(&chip, "pwm2");

        let result = fan.release();

        assert!(matches!(&result, Err(Error::Io { path, .. }) if *path == chip.path("pwm2")), "{:?}", result);
        assert_eq!(chip.read("pwm2_enable"), "5");
    }

    #[test]
    fn hwmon_release_returns_the_first_of_two_errors() {
        let chip = Nct6798::new("5\n", "127\n");
        let mut fan = chip.fan();
        fan.take_control().unwrap();
        break_attribute(&chip, "pwm2");
        break_attribute(&chip, "pwm2_enable");

        let result = fan.release();

        assert!(matches!(&result, Err(Error::Io { path, .. }) if *path == chip.path("pwm2")), "{:?}", result);
    }

    #[test]
    fn hwmon_second_release_writes_nothing() {
        let chip = Nct6798::new("5\n", "127\n");
        let mut fan = chip.fan();
        fan.take_control().unwrap();
        fan.release().unwrap();
        chip.write("pwm2", "90\n");
        chip.write("pwm2_enable", "2\n");

        fan.release().unwrap();

        assert_eq!(chip.read("pwm2"), "90\n");
        assert_eq!(chip.read("pwm2_enable"), "2\n");
    }

    #[test]
    fn hwmon_take_control_of_an_absent_chip_writes_nothing() {
        let chip = Nct6798::new("5\n", "127\n");
        let mut fan = HwmonPwm::new(HwmonRoot::new(chip.dir.0.clone()), "it8686".into(), 2);

        let error = fan.take_control().err().expect("no chip named it8686");
        fan.release().unwrap();

        assert!(matches!(&error, Error::HwmonChipNotFound { chip, .. } if chip == "it8686"), "{:?}", error);
        assert_eq!(chip.read("pwm2_enable"), "5\n");
        assert_eq!(chip.read("pwm2"), "127\n");
    }

    #[test]
    fn hwmon_take_control_writes_nothing_when_the_mode_cannot_be_read() {
        let chip = Nct6798::new("5\n", "127\n");
        fs::remove_file(chip.path("pwm2_enable")).unwrap();
        let mut fan = chip.fan();

        let result = fan.take_control();
        fan.release().unwrap();

        assert!(matches!(&result, Err(Error::Io { .. })), "{:?}", result);
        assert_eq!(chip.read("pwm2"), "127\n");
        assert!(!chip.path("pwm2_enable").exists());
    }

    #[test]
    fn hwmon_release_after_a_partial_take_control_restores_the_mode() {
        let chip = Nct6798::new("5\n", "127\n");
        break_attribute(&chip, "pwm2");
        let mut fan = chip.fan();

        let result = fan.take_control();
        let switched = chip.read("pwm2_enable");
        let released = fan.release();

        assert!(matches!(&result, Err(Error::Io { path, .. }) if *path == chip.path("pwm2")), "{:?}", result);
        assert_eq!(switched, "1");
        assert!(released.is_err());
        assert_eq!(chip.read("pwm2_enable"), "5");
    }

    #[test]
    fn hwmon_fan_keeps_the_device_it_took_over() {
        let chip = Nct6798::new("5\n", "127\n");
        let mut fan = chip.fan();
        fan.take_control().unwrap();
        fs::remove_dir_all(chip.dir.0.join("hwmon1")).unwrap();
        let moved = chip.dir.0.join("hwmon2");
        fs::create_dir(&moved).unwrap();
        fs::write(moved.join("name"), "nct6798\n").unwrap();
        fs::write(moved.join("pwm2_enable"), "1\n").unwrap();
        fs::write(moved.join("pwm2"), "77\n").unwrap();

        let duty = fan.set_duty(50);
        let released = fan.release();

        assert!(matches!(&duty, Err(Error::Io { .. })), "{:?}", duty);
        assert!(released.is_err());
        assert_eq!(fs::read_to_string(moved.join("pwm2")).unwrap(), "77\n");
        assert_eq!(fs::read_to_string(moved.join("pwm2_enable")).unwrap(), "1\n");
    }

    #[test]
    fn hwmon_fan_keeps_the_device_behind_the_link_it_took_over() {
        // As in sysfs, the root entry is a link to the chip's device directory
        let dir = TempDir::new();
        let (root, taken, other) = (dir.0.join("class"), dir.0.join("taken"), dir.0.join("other"));

        for device in [&root, &taken, &other] {
            fs::create_dir(device).unwrap();
        }

        for device in [&taken, &other] {
            fs::write(device.join("name"), "nct6798\n").unwrap();
            fs::write(device.join("pwm2_enable"), "5\n").unwrap();
            fs::write(device.join("pwm2"), "127\n").unwrap();
        }

        let link = root.join("hwmon1");
        symlink(&taken, &link).unwrap();
        let mut fan = HwmonPwm::new(HwmonRoot::new(root.clone()), "nct6798".into(), 2);
        fan.take_control().unwrap();
        fs::remove_file(&link).unwrap();
        symlink(&other, &link).unwrap();

        fan.set_duty(20).unwrap();

        assert_eq!(fs::read_to_string(taken.join("pwm2")).unwrap(), "51");
        assert_eq!(fs::read_to_string(other.join("pwm2")).unwrap(), "127\n");
    }

    #[test]
    fn hwmon_raw_value_above_full_is_an_error() {
        let chip = Nct6798::new("5\n", "127\n");
        let mut fan = chip.fan();
        fan.take_control().unwrap();
        chip.write("pwm2", "300\n");

        let result = fan.set_duty(50);

        assert!(matches!(&result, Err(Error::HwmonValueOutOfRange { value: 300, .. })), "{:?}", result);
        assert_eq!(chip.read("pwm2"), "300\n");
    }

    #[test]
    fn hwmon_fan_is_built_without_a_session() {
        let config = FanConfig::Hwmon { chip: "nct6798".into(), pwm: 2 };

        let fan = build_fan(&config, None, &HwmonRoot::default(), &no_nvml()).unwrap();

        assert_eq!(fan.describe(), "hwmon nct6798 pwm2");
    }

    const GPU: &str = "00000000:01:00.0";

    /// One fan at 20% with limits of 30% and 100%, as on an RTX A4000
    const ONE_FAN: FakeGpu = FakeGpu { fans: 1, limits: (30, 100), speed: 20 };

    /// An NVML that holds [`GPU`], which reports `reports`
    fn nvml_with(reports: FakeGpu) -> Arc<FakeNvml> {
        Arc::new(FakeNvml::with_gpu(GPU, reports))
    }

    fn nvidia_fan(nvml: &Arc<FakeNvml>, gpu: &str, fan: u32) -> NvidiaFan {
        NvidiaFan::new(nvml.clone(), gpu.into(), fan)
    }

    /// The setter calls of `nvml`, which are the calls that change a fan
    fn sets_of(nvml: &FakeNvml) -> Vec<Call> {
        nvml.calls().into_iter()
            .filter(|call| matches!(call, Call::SetFanSpeed(..) | Call::SetDefaultFanSpeed(..)))
            .collect()
    }

    #[test]
    fn nvidia_set_duty_raises_to_the_minimum_and_sends_only_changes() {
        let nvml = nvml_with(ONE_FAN);
        let mut fan = nvidia_fan(&nvml, GPU, 0);
        fan.take_control().unwrap();

        let first = fan.set_duty(20).unwrap();
        let repeated = fan.set_duty(20).unwrap();
        let raised = fan.set_duty(65).unwrap();

        assert_eq!(first, Applied { before: Some(20), sent: Some(30) });
        assert_eq!(repeated, Applied { before: Some(20), sent: None });
        assert_eq!(raised, Applied { before: Some(20), sent: Some(65) });
        assert_eq!(sets_of(&nvml), [Call::SetFanSpeed(GPU.into(), 0, 30), Call::SetFanSpeed(GPU.into(), 0, 65)]);
    }

    #[test]
    fn nvidia_set_duty_caps_at_the_maximum() {
        let nvml = nvml_with(FakeGpu { limits: (30, 80), ..ONE_FAN });
        let mut fan = nvidia_fan(&nvml, GPU, 0);
        fan.take_control().unwrap();

        let applied = fan.set_duty(100).unwrap();

        assert_eq!(applied.sent, Some(80));
        assert_eq!(sets_of(&nvml), [Call::SetFanSpeed(GPU.into(), 0, 80)]);
    }

    #[test]
    fn nvidia_first_set_duty_sends_even_the_reported_speed() {
        let nvml = nvml_with(FakeGpu { speed: 50, ..ONE_FAN });
        let mut fan = nvidia_fan(&nvml, GPU, 0);
        fan.take_control().unwrap();

        let applied = fan.set_duty(50).unwrap();

        assert_eq!(applied, Applied { before: Some(50), sent: Some(50) });
        assert_eq!(sets_of(&nvml), [Call::SetFanSpeed(GPU.into(), 0, 50)]);
    }

    #[test]
    fn nvidia_set_duty_sends_again_after_a_failed_send() {
        let failing = FakeNvml { failing: Some(NvmlOp::SetFanSpeed), ..FakeNvml::with_gpu(GPU, ONE_FAN) };
        let nvml = Arc::new(failing);
        let mut fan = nvidia_fan(&nvml, GPU, 0);
        fan.take_control().unwrap();

        let first = fan.set_duty(40);
        let second = fan.set_duty(40);

        assert!(matches!(&first, Err(Error::Nvml { gpu, .. }) if gpu == GPU), "{:?}", first);
        assert!(second.is_err(), "{:?}", second);
        assert_eq!(sets_of(&nvml), [Call::SetFanSpeed(GPU.into(), 0, 40), Call::SetFanSpeed(GPU.into(), 0, 40)]);
    }

    #[test]
    fn nvidia_set_duty_before_take_control_sends_nothing() {
        let nvml = nvml_with(ONE_FAN);
        let mut fan = nvidia_fan(&nvml, GPU, 0);

        let result = fan.set_duty(50);

        assert!(matches!(&result, Err(Error::FanNotTaken(_))), "{:?}", result);
        assert_eq!(sets_of(&nvml), []);
    }

    #[test]
    fn nvidia_release_asks_for_the_default_policy_once() {
        let nvml = nvml_with(ONE_FAN);
        let mut fan = nvidia_fan(&nvml, GPU, 0);
        fan.take_control().unwrap();
        fan.set_duty(50).unwrap();

        fan.release().unwrap();
        fan.release().unwrap();

        assert_eq!(sets_of(&nvml), [Call::SetFanSpeed(GPU.into(), 0, 50), Call::SetDefaultFanSpeed(GPU.into(), 0)]);
    }

    #[test]
    fn nvidia_release_sets_the_maximum_when_the_default_policy_fails() {
        let failing = FakeNvml { failing: Some(NvmlOp::SetDefaultFanSpeed), ..FakeNvml::with_gpu(GPU, FakeGpu { limits: (30, 90), ..ONE_FAN }) };
        let nvml = Arc::new(failing);
        let mut fan = nvidia_fan(&nvml, GPU, 0);
        fan.take_control().unwrap();
        fan.set_duty(40).unwrap();

        let result = fan.release();

        assert!(matches!(&result, Err(Error::Nvml { gpu, .. }) if gpu == GPU), "{:?}", result);
        assert_eq!(sets_of(&nvml)[1..], [Call::SetDefaultFanSpeed(GPU.into(), 0), Call::SetFanSpeed(GPU.into(), 0, 90)]);
    }

    #[test]
    fn nvidia_take_control_of_a_missing_fan_fails() {
        let nvml = nvml_with(ONE_FAN);
        let mut fan = nvidia_fan(&nvml, GPU, 1);

        let result = fan.take_control();
        fan.release().unwrap();

        let Err(error) = result else {
            panic!("expected an error, got {:?}", result);
        };
        assert!(matches!(&error, Error::NvidiaFanMissing { gpu, fan: 1, count: 1 } if gpu == GPU), "{:?}", error);
        assert!(error.to_string().contains(GPU), "{}", error);
        assert!(error.to_string().contains("has 1 fan"), "{}", error);
        assert_eq!(sets_of(&nvml), []);
    }

    #[test]
    fn nvidia_take_control_of_an_unknown_bus_id_fails() {
        let nvml = nvml_with(ONE_FAN);
        let mut fan = nvidia_fan(&nvml, "00000000:02:00.0", 0);

        let result = fan.take_control();
        fan.release().unwrap();

        let Err(error) = result else {
            panic!("expected an error, got {:?}", result);
        };
        assert!(error.to_string().contains("00000000:02:00.0"), "{}", error);
        assert_eq!(sets_of(&nvml), []);
    }

    #[test]
    fn nvidia_release_without_take_control_sends_nothing() {
        let nvml = nvml_with(ONE_FAN);
        let mut fan = nvidia_fan(&nvml, GPU, 0);

        fan.release().unwrap();

        assert_eq!(nvml.calls(), []);
    }

    #[test]
    fn nvidia_take_control_rejects_inverted_limits() {
        let nvml = nvml_with(FakeGpu { limits: (60, 40), ..ONE_FAN });
        let mut fan = nvidia_fan(&nvml, GPU, 0);

        let result = fan.take_control();
        fan.release().unwrap();

        assert!(matches!(&result, Err(Error::NvidiaFanLimits { min: 60, max: 40, .. })), "{:?}", result);
        assert_eq!(sets_of(&nvml), []);
    }

    #[test]
    fn nvidia_take_control_rejects_a_maximum_above_100() {
        let nvml = nvml_with(FakeGpu { limits: (30, 101), ..ONE_FAN });
        let mut fan = nvidia_fan(&nvml, GPU, 0);

        let result = fan.take_control();
        fan.release().unwrap();

        assert!(matches!(&result, Err(Error::NvidiaFanLimits { min: 30, max: 101, .. })), "{:?}", result);
        assert_eq!(sets_of(&nvml), []);
    }

    #[test]
    fn dropped_guard_releases_an_nvidia_fan() {
        let nvml = nvml_with(ONE_FAN);

        {
            let mut guard = FanGuard::new();
            guard.add(Box::new(nvidia_fan(&nvml, GPU, 0))).unwrap();
            guard.set_duty(50).unwrap();
        }

        assert_eq!(sets_of(&nvml), [Call::SetFanSpeed(GPU.into(), 0, 50), Call::SetDefaultFanSpeed(GPU.into(), 0)]);
    }

    /// NVML limits short enough for a test
    const SHORT: Limits = Limits { call: Duration::from_millis(200), load: Duration::from_millis(400) };

    /// The longest a call that fails at once may take
    const AT_ONCE: Duration = Duration::from_millis(100);

    /// An NVML worker over a fake that holds [`GPU`] and whose fan_speed never returns
    fn stalling_on_fan_speed() -> Arc<dyn Nvml> {
        Arc::new(stalling_worker(GPU, ONE_FAN, NvmlOp::FanSpeed, SHORT))
    }

    #[test]
    fn nvidia_release_after_a_timeout_fails_at_once() {
        let nvml = stalling_on_fan_speed();
        let mut fan = NvidiaFan::new(nvml.clone(), GPU.into(), 0);
        fan.take_control().unwrap();
        let timed_out = nvml.fan_speed(GPU, 0);

        let start = Instant::now();
        let result = fan.release();
        let elapsed = start.elapsed();

        assert!(matches!(&timed_out, Err(Error::NvmlTimeout { .. })), "{:?}", timed_out);
        assert!(matches!(&result, Err(Error::NvmlStalled { .. })), "{:?}", result);
        assert!(elapsed < AT_ONCE, "{:?}", elapsed);
    }

    #[test]
    fn dropped_guard_releases_another_fan_after_an_nvidia_timeout() {
        let calls = CallLog::default();
        let nvml = stalling_on_fan_speed();
        let mut guard = FanGuard::new();
        guard.add(Box::new(FakeFan::new("a", &calls))).unwrap();
        // Added last, so released first
        guard.add(Box::new(NvidiaFan::new(nvml, GPU.into(), 0))).unwrap();

        let duty = guard.set_duty(50);
        let start = Instant::now();
        drop(guard);
        let elapsed = start.elapsed();

        assert!(duty.is_err(), "{:?}", duty);
        assert_eq!(log_of(&calls), [("a", Op::TakeControl), ("a", Op::SetDuty), ("a", Op::Release)]);
        assert!(elapsed < AT_ONCE, "{:?}", elapsed);
    }

    #[test]
    fn nvidia_fan_is_built_without_a_session() {
        let nvml = nvml_with(ONE_FAN);
        let shared: Arc<dyn Nvml> = nvml.clone();
        let config = FanConfig::Nvidia { gpu: GPU.into(), fan: 0 };

        let mut fan = build_fan(&config, None, &HwmonRoot::default(), &shared).unwrap();
        fan.take_control().unwrap();

        assert_eq!(fan.describe(), "nvidia gpu 00000000:01:00.0 fan 0");
        assert_eq!(nvml.calls()[0], Call::FanCount(GPU.into()));
    }
}
