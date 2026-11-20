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

use {
    std::{
        ffi::OsStr,
        sync::{Mutex, PoisonError},
    },
    nvml_wrapper::{error::NvmlError, Device, Nvml as NvmlLibrary},
    crate::error::{Error, Result},
};

/// The NVML library of the host's NVIDIA driver. It must match the loaded
/// kernel driver, so it is loaded at run time and is not a build input.
pub const NVML_LIBRARY: &str = "libnvidia-ml.so.1";

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

/// NVML through the host's library, which is loaded on first use. A config
/// with no NVIDIA fan never makes a call, so it never loads the library.
pub struct RealNvml {
    library: String,
    loaded: Mutex<Option<NvmlLibrary>>,
}

impl RealNvml {
    pub fn new() -> Self {
        Self::with_library(NVML_LIBRARY)
    }

    /// NVML loaded from `library`, a file name or a path
    pub fn with_library(library: &str) -> Self {
        Self { library: library.to_owned(), loaded: Mutex::new(None) }
    }

    /// Run `call` on a fresh handle of the GPU at bus id `gpu`, and load the
    /// library first if no call has loaded it yet. A failed load is tried
    /// again by the next call. The setters need a mutable `Device`, and a
    /// `Device` borrows the library, so no handle outlives one call.
    fn with_device<T>(&self, gpu: &str, call: impl FnOnce(&mut Device) -> Result<T, NvmlError>) -> Result<T> {
        // A panic while the lock was held leaves no partial state behind:
        // the slot holds a loaded library or nothing
        let mut loaded = self.loaded.lock().unwrap_or_else(PoisonError::into_inner);
        let library = match &mut *loaded {
            Some(library) => library,
            empty => empty.insert(self.load(gpu)?),
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

impl Nvml for RealNvml {
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
        std::{collections::HashMap, sync::Mutex},
        nvml_wrapper::error::NvmlError,
        super::Nvml,
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
        calls: Mutex<Vec<Call>>,
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
}

#[cfg(test)]
mod tests {
    use {
        super::{Nvml, RealNvml},
        crate::error::Error,
    };

    /// A library name that no system has, so no test loads a real NVML
    const MISSING_LIBRARY: &str = "libnvidia-ml-coolerd-test-missing.so.1";

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
}
