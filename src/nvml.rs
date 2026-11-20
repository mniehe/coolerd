// The driver layer for NVIDIA GPU fans. It uses these NVML functions, through
// the nvml-wrapper crate:
//
// * nvmlDeviceGetHandleByPciBusId_v2: find the GPU by PCI bus id
// * nvmlDeviceGetNumFans: the number of fans of the GPU
// * nvmlDeviceGetMinMaxFanSpeed: the lowest and highest speed a fan accepts
// * nvmlDeviceGetFanSpeed_v2: the speed a fan reports, in percent
// * nvmlDeviceSetFanSpeed_v2: set a fan's speed, which switches it to manual
// * nvmlDeviceSetDefaultFanSpeed_v2: give a fan back to the driver's policy
//
// The crate holds every unsafe FFI call, so this module has no unsafe block.
//
// A call into a hung driver may never return, and nothing can cancel it. So
// one worker thread makes every call, and a caller waits for the reply only
// up to a limit. After one call times out, the worker may stay blocked
// forever, and every later call fails at once instead of queueing behind it.

use {
    std::{
        ffi::OsStr,
        mem::ManuallyDrop,
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc::{self, RecvTimeoutError, Sender},
            Arc,
            OnceLock,
        },
        thread,
        time::Duration,
    },
    nvml_wrapper::{error::NvmlError, Device, Nvml as NvmlLibrary},
    crate::error::{Error, Result},
};

/// The NVML library of the host's NVIDIA driver. It must match the loaded
/// kernel driver, so it is loaded at run time and is not a build input.
pub const NVML_LIBRARY: &str = "libnvidia-ml.so.1";

/// The longest wait for the reply to one NVML call, the same limit that
/// nvidia-smi gets
pub const NVML_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// The longest wait for a call that must load the library first, because
/// nvmlInit can take much longer than a call
pub const NVML_LOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// The name of the worker thread, as it shows in a debugger or in /proc
const WORKER_NAME: &str = "nvml";

/// How long a caller waits for the worker's reply
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// For a call once the library is loaded
    pub call: Duration,
    /// For a call that may have to load the library first
    pub load: Duration,
}

const LIMITS: Limits = Limits { call: NVML_CALL_TIMEOUT, load: NVML_LOAD_TIMEOUT };

/// The fan operations of NVML. Each call selects its GPU by PCI bus id.
pub trait Nvml: Send + Sync {
    /// The number of fans of `gpu`
    fn fan_count(&self, gpu: &str) -> Result<u32>;
    /// The lowest and highest speed, in percent, that the fans of `gpu` accept
    fn fan_limits(&self, gpu: &str) -> Result<(u32, u32)>;
    /// The speed that fan `fan` of `gpu` reports, in percent
    fn fan_speed(&self, gpu: &str, fan: u32) -> Result<u32>;
    /// Set fan `fan` of `gpu` to `percent` under manual control
    fn set_fan_speed(&self, gpu: &str, fan: u32, percent: u32) -> Result<()>;
    /// Give fan `fan` of `gpu` back to the driver's default fan policy
    fn set_default_fan_speed(&self, gpu: &str, fan: u32) -> Result<()>;
}

/// Builds the backend that the worker thread then owns and calls
type OpenBackend = Arc<dyn Fn() -> Box<dyn Nvml> + Send + Sync>;

/// One call for the worker thread to make on its backend
type Job = Box<dyn FnOnce(&dyn Nvml) + Send>;

/// NVML through the host's library, called on one worker thread. The thread
/// starts on the first call and loads the library then, so a config with no
/// NVIDIA fan never starts it. Dropping this value closes the worker's queue
/// and never waits for the thread.
pub struct RealNvml {
    open: OpenBackend,
    limits: Limits,
    /// The worker's queue, once the worker has started
    jobs: OnceLock<Sender<Job>>,
    /// Whether a call has returned anything other than a failed load, which
    /// means the library is loaded
    loaded: AtomicBool,
    /// The call that timed out, once one has
    stalled: OnceLock<String>,
}

impl RealNvml {
    pub fn new() -> Self {
        Self::with_library(NVML_LIBRARY)
    }

    /// NVML loaded from `library`, a file name or a path
    pub fn with_library(library: &str) -> Self {
        let library = library.to_owned();

        Self::with_backend(Arc::new(move || Box::new(LibraryNvml::new(&library))), LIMITS)
    }

    /// A worker over the backend that `open` builds, with the waits `limits`
    fn with_backend(open: OpenBackend, limits: Limits) -> Self {
        Self { open, limits, jobs: OnceLock::new(), loaded: AtomicBool::new(false), stalled: OnceLock::new() }
    }

    /// The worker's queue. Starts the worker on the first call.
    fn jobs(&self, gpu: &str) -> Result<&Sender<Job>> {
        if let Some(jobs) = self.jobs.get() {
            return Ok(jobs);
        }

        let (jobs, queue) = mpsc::channel::<Job>();
        let open = self.open.clone();

        thread::Builder::new()
            .name(WORKER_NAME.to_owned())
            .spawn(move || {
                let backend = open();

                for job in queue {
                    job(&*backend);
                }
            })
            .map_err(|source| Error::NvmlThread { gpu: gpu.to_owned(), source })?;

        // Two first calls at once can each start a worker. The worker whose
        // queue is not kept sees it close and ends before it loads anything.
        Ok(self.jobs.get_or_init(|| jobs))
    }

    /// Make `call` on the worker's backend and wait for its result, for
    /// [`Limits::load`] until the library is loaded and [`Limits::call`] after
    fn request<T: Send + 'static>(
        &self,
        gpu: &str,
        operation: &'static str,
        call: impl FnOnce(&dyn Nvml) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        if let Some(earlier) = self.stalled.get() {
            return Err(Error::NvmlStalled { gpu: gpu.to_owned(), operation, earlier: earlier.clone() });
        }

        let limit = if self.loaded.load(Ordering::SeqCst) { self.limits.call } else { self.limits.load };
        // Room for the one reply, so the worker never waits for a caller
        let (reply, replies) = mpsc::sync_channel(1);
        let job: Job = Box::new(move |backend| {
            // A caller that timed out no longer receives
            let _ = reply.send(call(backend));
        });
        let no_reply = || Error::NvmlNoReply { gpu: gpu.to_owned(), operation };

        self.jobs(gpu)?.send(job).map_err(|_| no_reply())?;

        match replies.recv_timeout(limit) {
            Ok(result) => {
                if !matches!(result, Err(Error::NvmlLoad { .. })) {
                    self.loaded.store(true, Ordering::SeqCst);
                }

                result
            }
            Err(RecvTimeoutError::Timeout) => {
                let _ = self.stalled.set(format!("GPU {}: {}", gpu, operation));

                Err(Error::NvmlTimeout { gpu: gpu.to_owned(), operation, limit })
            }
            Err(RecvTimeoutError::Disconnected) => Err(no_reply()),
        }
    }
}

impl Nvml for RealNvml {
    fn fan_count(&self, gpu: &str) -> Result<u32> {
        let bus_id = gpu.to_owned();
        self.request(gpu, "fan_count", move |nvml| nvml.fan_count(&bus_id))
    }

    fn fan_limits(&self, gpu: &str) -> Result<(u32, u32)> {
        let bus_id = gpu.to_owned();
        self.request(gpu, "fan_limits", move |nvml| nvml.fan_limits(&bus_id))
    }

    fn fan_speed(&self, gpu: &str, fan: u32) -> Result<u32> {
        let bus_id = gpu.to_owned();
        self.request(gpu, "fan_speed", move |nvml| nvml.fan_speed(&bus_id, fan))
    }

    fn set_fan_speed(&self, gpu: &str, fan: u32, percent: u32) -> Result<()> {
        let bus_id = gpu.to_owned();
        self.request(gpu, "set_fan_speed", move |nvml| nvml.set_fan_speed(&bus_id, fan, percent))
    }

    fn set_default_fan_speed(&self, gpu: &str, fan: u32) -> Result<()> {
        let bus_id = gpu.to_owned();
        self.request(gpu, "set_default_fan_speed", move |nvml| nvml.set_default_fan_speed(&bus_id, fan))
    }
}

/// NVML called directly through the host's library, which is loaded on the
/// first call. Only the worker thread holds one.
struct LibraryNvml {
    library: String,
    /// Never dropped. Its drop calls nvmlShutdown, which can hang in the
    /// same driver as any other call, and nvml-wrapper looks the symbol up
    /// through an `expect` that panics when the library lacks it. The
    /// process exit frees the library instead.
    loaded: OnceLock<ManuallyDrop<NvmlLibrary>>,
}

impl LibraryNvml {
    fn new(library: &str) -> Self {
        Self { library: library.to_owned(), loaded: OnceLock::new() }
    }

    /// Run `call` on a fresh handle of the GPU at bus id `gpu`, and load the
    /// library first if no call has loaded it yet. A failed load is tried
    /// again by the next call. The setters need a mutable `Device`, and a
    /// `Device` borrows the library, so no handle outlives one call.
    fn with_device<T>(&self, gpu: &str, call: impl FnOnce(&mut Device) -> Result<T, NvmlError>) -> Result<T> {
        let library = match self.loaded.get() {
            Some(library) => library,
            None => {
                let library = ManuallyDrop::new(self.load(gpu)?);
                self.loaded.get_or_init(|| library)
            }
        };
        let nvml_error = |source| Error::Nvml { gpu: gpu.to_owned(), source };
        let mut device = library.device_by_pci_bus_id(gpu).map_err(nvml_error)?;

        call(&mut device).map_err(nvml_error)
    }

    fn load(&self, gpu: &str) -> Result<NvmlLibrary> {
        NvmlLibrary::builder()
            .lib_path(OsStr::new(&self.library))
            .init()
            .map_err(|source| Error::NvmlLoad { gpu: gpu.to_owned(), library: self.library.clone(), source })
    }
}

impl Nvml for LibraryNvml {
    fn fan_count(&self, gpu: &str) -> Result<u32> {
        self.with_device(gpu, |device| device.num_fans())
    }

    fn fan_limits(&self, gpu: &str) -> Result<(u32, u32)> {
        self.with_device(gpu, |device| device.min_max_fan_speed())
    }

    fn fan_speed(&self, gpu: &str, fan: u32) -> Result<u32> {
        self.with_device(gpu, |device| device.fan_speed(fan))
    }

    fn set_fan_speed(&self, gpu: &str, fan: u32, percent: u32) -> Result<()> {
        self.with_device(gpu, |device| device.set_fan_speed(fan, percent))
    }

    fn set_default_fan_speed(&self, gpu: &str, fan: u32) -> Result<()> {
        self.with_device(gpu, |device| device.set_default_fan_speed(fan))
    }
}

#[cfg(test)]
pub mod fake {
    use {
        std::{
            collections::HashMap,
            sync::{
                mpsc::{self, Receiver, Sender},
                Arc,
                Mutex,
            },
        },
        nvml_wrapper::error::NvmlError,
        super::{Limits, Nvml, RealNvml},
        crate::error::{Error, Result},
    };

    /// An operation of the [`Nvml`] trait
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum Op {
        FanCount,
        FanLimits,
        FanSpeed,
        SetFanSpeed,
        SetDefaultFanSpeed,
    }

    /// One call to a fake NVML, with its arguments
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub enum Call {
        FanCount(String),
        FanLimits(String),
        FanSpeed(String, u32),
        SetFanSpeed(String, u32, u32),
        SetDefaultFanSpeed(String, u32),
    }

    /// What a fake GPU reports
    #[derive(Clone, Copy, Debug)]
    pub struct FakeGpu {
        pub fans: u32,
        pub limits: (u32, u32),
        pub speed: u32,
    }

    /// An NVML that records its calls. A GPU it does not hold is not found,
    /// as NVML reports an unknown bus id. One operation can fail.
    #[derive(Default)]
    pub struct FakeNvml {
        pub gpus: HashMap<String, FakeGpu>,
        pub failing: Option<Op>,
        pub calls: Mutex<Vec<Call>>,
    }

    impl FakeNvml {
        /// A fake NVML that holds one GPU
        pub fn with_gpu(gpu: &str, reports: FakeGpu) -> Self {
            Self { gpus: HashMap::from([(gpu.to_owned(), reports)]), ..Self::default() }
        }

        /// Every call so far, in order
        pub fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        /// Record `call`, then fail it when `op` fails, or find `gpu`
        fn call(&self, op: Op, gpu: &str, call: Call) -> Result<FakeGpu> {
            self.calls.lock().unwrap().push(call);

            if self.failing == Some(op) {
                return Err(Error::Nvml { gpu: gpu.to_owned(), source: NvmlError::Unknown });
            }

            self.gpus.get(gpu).copied()
                .ok_or_else(|| Error::Nvml { gpu: gpu.to_owned(), source: NvmlError::NotFound })
        }
    }

    impl Nvml for FakeNvml {
        fn fan_count(&self, gpu: &str) -> Result<u32> {
            Ok(self.call(Op::FanCount, gpu, Call::FanCount(gpu.to_owned()))?.fans)
        }

        fn fan_limits(&self, gpu: &str) -> Result<(u32, u32)> {
            Ok(self.call(Op::FanLimits, gpu, Call::FanLimits(gpu.to_owned()))?.limits)
        }

        fn fan_speed(&self, gpu: &str, fan: u32) -> Result<u32> {
            Ok(self.call(Op::FanSpeed, gpu, Call::FanSpeed(gpu.to_owned(), fan))?.speed)
        }

        fn set_fan_speed(&self, gpu: &str, fan: u32, percent: u32) -> Result<()> {
            self.call(Op::SetFanSpeed, gpu, Call::SetFanSpeed(gpu.to_owned(), fan, percent)).map(|_| ())
        }

        fn set_default_fan_speed(&self, gpu: &str, fan: u32) -> Result<()> {
            self.call(Op::SetDefaultFanSpeed, gpu, Call::SetDefaultFanSpeed(gpu.to_owned(), fan)).map(|_| ())
        }
    }

    /// A [`FakeNvml`] whose `stalls` operation never returns, as a call into
    /// a hung driver: it waits on a channel whose sender it keeps and never
    /// uses
    struct StallingNvml {
        inner: FakeNvml,
        stalls: Op,
        never: Mutex<Receiver<()>>,
        _sender: Sender<()>,
    }

    impl StallingNvml {
        fn pass<T>(&self, op: Op, call: impl FnOnce(&FakeNvml) -> Result<T>) -> Result<T> {
            if op == self.stalls {
                let _ = self.never.lock().unwrap().recv();
            }

            call(&self.inner)
        }
    }

    impl Nvml for StallingNvml {
        fn fan_count(&self, gpu: &str) -> Result<u32> {
            self.pass(Op::FanCount, |nvml| nvml.fan_count(gpu))
        }

        fn fan_limits(&self, gpu: &str) -> Result<(u32, u32)> {
            self.pass(Op::FanLimits, |nvml| nvml.fan_limits(gpu))
        }

        fn fan_speed(&self, gpu: &str, fan: u32) -> Result<u32> {
            self.pass(Op::FanSpeed, |nvml| nvml.fan_speed(gpu, fan))
        }

        fn set_fan_speed(&self, gpu: &str, fan: u32, percent: u32) -> Result<()> {
            self.pass(Op::SetFanSpeed, |nvml| nvml.set_fan_speed(gpu, fan, percent))
        }

        fn set_default_fan_speed(&self, gpu: &str, fan: u32) -> Result<()> {
            self.pass(Op::SetDefaultFanSpeed, |nvml| nvml.set_default_fan_speed(gpu, fan))
        }
    }

    /// The worker NVML of production, with the waits `limits`, over a fake
    /// that holds `gpu` and never returns from `stalls`. A stalled worker
    /// thread stays blocked until the test process exits.
    pub fn stalling_worker(gpu: &str, reports: FakeGpu, stalls: Op, limits: Limits) -> RealNvml {
        let gpu = gpu.to_owned();

        RealNvml::with_backend(Arc::new(move || {
            let (sender, never) = mpsc::channel();
            let inner = FakeNvml::with_gpu(&gpu, reports);

            Box::new(StallingNvml { inner, stalls, never: Mutex::new(never), _sender: sender })
        }), limits)
    }
}

#[cfg(test)]
mod tests {
    use {
        std::time::{Duration, Instant},
        super::{
            fake::{stalling_worker, FakeGpu, Op},
            Limits,
            Nvml,
            RealNvml,
        },
        crate::error::Error,
    };

    /// A library name that no system has, so no test loads a real NVML
    const MISSING_LIBRARY: &str = "libnvidia-ml-coolerd-test-missing.so.1";

    const GPU: &str = "00000000:01:00.0";

    const ONE_FAN: FakeGpu = FakeGpu { fans: 1, limits: (30, 100), speed: 20 };

    /// Limits short enough for a test, with a longer load limit as in production
    const SHORT: Limits = Limits { call: Duration::from_millis(200), load: Duration::from_millis(400) };

    /// The longest a call that fails at once may take
    const AT_ONCE: Duration = Duration::from_millis(100);

    /// Slack for a busy test machine on top of a limit
    const SLACK: Duration = Duration::from_secs(2);

    /// A worker over a fake whose fan_speed never returns, with the library
    /// loaded by a first call
    fn loaded_worker_stalling_on_fan_speed() -> RealNvml {
        let nvml = stalling_worker(GPU, ONE_FAN, Op::FanSpeed, SHORT);
        assert_eq!(nvml.fan_count(GPU).unwrap(), 1);
        nvml
    }

    #[test]
    fn missing_library_is_an_error_that_names_it() {
        let nvml = RealNvml::with_library(MISSING_LIBRARY);

        let result = nvml.fan_count("00000000:01:00.0");

        let Err(error) = result else {
            panic!("expected an error, got {:?}", result);
        };
        assert!(matches!(&error, Error::NvmlLoad { library, gpu, .. }
            if library == MISSING_LIBRARY && gpu == "00000000:01:00.0"), "{:?}", error);
        assert!(error.to_string().contains(MISSING_LIBRARY), "{}", error);
    }

    #[test]
    fn failed_load_is_tried_again_by_the_next_call() {
        let nvml = RealNvml::with_library(MISSING_LIBRARY);

        let first = nvml.fan_count(GPU);
        let second = nvml.fan_limits(GPU);

        assert!(matches!(&first, Err(Error::NvmlLoad { .. })), "{:?}", first);
        assert!(matches!(&second, Err(Error::NvmlLoad { .. })), "{:?}", second);
    }

    #[test]
    fn worker_returns_the_backend_results() {
        let nvml = stalling_worker(GPU, ONE_FAN, Op::SetDefaultFanSpeed, SHORT);

        assert_eq!(nvml.fan_count(GPU).unwrap(), 1);
        assert_eq!(nvml.fan_limits(GPU).unwrap(), (30, 100));
        assert_eq!(nvml.fan_speed(GPU, 0).unwrap(), 20);
        nvml.set_fan_speed(GPU, 0, 40).unwrap();
        let unknown = nvml.fan_count("00000000:02:00.0");
        assert!(matches!(&unknown, Err(Error::Nvml { gpu, .. }) if gpu == "00000000:02:00.0"), "{:?}", unknown);
    }

    #[test]
    fn stalled_call_times_out_after_the_call_limit() {
        let nvml = loaded_worker_stalling_on_fan_speed();

        let start = Instant::now();
        let result = nvml.fan_speed(GPU, 0);
        let elapsed = start.elapsed();

        let Err(error) = result else {
            panic!("expected a timeout, got {:?}", result);
        };
        assert!(matches!(&error, Error::NvmlTimeout { gpu, operation: "fan_speed", limit }
            if gpu == GPU && *limit == SHORT.call), "{:?}", error);
        assert!(error.to_string().contains(GPU) && error.to_string().contains("fan_speed"), "{}", error);
        assert!(elapsed >= SHORT.call && elapsed < SHORT.call + SLACK, "{:?}", elapsed);
    }

    #[test]
    fn first_call_waits_for_the_load_limit() {
        let nvml = stalling_worker(GPU, ONE_FAN, Op::FanCount, SHORT);

        let start = Instant::now();
        let result = nvml.fan_count(GPU);
        let elapsed = start.elapsed();

        assert!(matches!(&result, Err(Error::NvmlTimeout { operation: "fan_count", limit, .. })
            if *limit == SHORT.load), "{:?}", result);
        assert!(elapsed >= SHORT.load && elapsed < SHORT.load + SLACK, "{:?}", elapsed);
    }

    #[test]
    fn every_call_after_a_timeout_fails_at_once() {
        let nvml = loaded_worker_stalling_on_fan_speed();
        let _ = nvml.fan_speed(GPU, 0);

        let start = Instant::now();
        let count = nvml.fan_count(GPU);
        let set = nvml.set_default_fan_speed(GPU, 0);
        let elapsed = start.elapsed();

        assert!(matches!(&count, Err(Error::NvmlStalled { gpu, operation: "fan_count", .. }) if gpu == GPU), "{:?}", count);
        assert!(matches!(&set, Err(Error::NvmlStalled { operation: "set_default_fan_speed", .. })), "{:?}", set);
        assert!(elapsed < AT_ONCE, "{:?}", elapsed);
    }

    #[test]
    fn drop_after_a_timeout_does_not_wait_for_the_worker() {
        let nvml = loaded_worker_stalling_on_fan_speed();
        let _ = nvml.fan_speed(GPU, 0);

        let start = Instant::now();
        drop(nvml);

        assert!(start.elapsed() < AT_ONCE, "{:?}", start.elapsed());
    }
}
