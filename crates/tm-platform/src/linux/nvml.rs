//! Dynamic loader and cached enumeration for NVML, the NVIDIA management
//! library.
//!
//! `libnvidia-ml.so.1` is neither a build dependency nor present on every
//! machine, so it is opened at runtime (`dlopen`/`dlsym`, no crate, no link
//! flag) and the entry points are bound to the ABI shapes in
//! [`crate::linux_sensors`]. Loading happens once per process: a missing
//! library or a failed `nvmlInit_v2` is a permanent "not available", because
//! the sampler asks at 1 Hz and a retry every tick would be a storm instead of
//! a recovery.
//!
//! NVML reports per-adapter capability (a laptop dGPU has no fan, a vGPU no PCI
//! id), so every refused query — `NVML_ERROR_NOT_SUPPORTED` included — leaves
//! that field absent. Nothing here invents a value.

use crate::linux_sensors::{
    GpuSensors, NVML_CLOCK_GRAPHICS, NVML_CLOCK_MEM, NVML_ERROR_NO_PERMISSION,
    NVML_ERROR_NOT_SUPPORTED, NVML_SUCCESS, NVML_TEMPERATURE_GPU, NVML_TEMPERATURE_MEMORY,
    NvmlMemory, NvmlPciInfo, NvmlUtilization, nvml_bounded, nvml_clock_mhz,
    nvml_milliwatts_to_watts, nvml_percent, nvml_temperature_c, nvml_text,
};
use std::ffi::{CString, c_char, c_int, c_uint, c_ulonglong, c_void};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// `dlopen` mode from `<dlfcn.h>`, which `libc` does not expose for Linux.
/// Every Linux libc (glibc, musl) uses 2.
const RTLD_NOW: c_int = 0x2;
/// `NVML_DEVICE_NAME_V2_BUFFER_SIZE` and
/// `NVML_SYSTEM_DRIVER_VERSION_BUFFER_SIZE`.
const NAME_BUFFER: usize = 96;
const DRIVER_VERSION_BUFFER: usize = 80;
/// A host with more than a couple of GPUs is already exotic, and NVML itself
/// never reports more than a few dozen; the cap keeps a hostile library from
/// making the enumeration loop unbounded.
const MAX_DEVICES: u32 = 32;
/// One sampling tick asks once, and the answer describes every adapter, so a
/// tick never calls the driver once per card.
const CACHE_TTL: Duration = Duration::from_secs(1);
/// The driver's control device. It exists only while the kernel module is
/// loaded, and its mode decides whether an unprivileged token may use NVML at
/// all (Debian and Ubuntu ship it group `video` readable).
const CONTROL_DEVICE: &str = "/dev/nvidiactl";

/// Why `nvmlInit_v2` failed, kept so the fallback decision never re-probes the
/// driver. Set once, before `library()` can answer `None`.
static INIT_CODE: OnceLock<c_int> = OnceLock::new();

/// `nvmlInit_v2`, `nvmlShutdown`.
type Init = unsafe extern "C" fn() -> c_int;
/// `nvmlDeviceGetCount_v2`.
type DeviceCount = unsafe extern "C" fn(*mut c_uint) -> c_int;
/// `nvmlDeviceGetHandleByIndex_v2`.
type HandleByIndex = unsafe extern "C" fn(c_uint, *mut *mut c_void) -> c_int;
/// `nvmlDeviceGetPciInfo_v3` / `nvmlDeviceGetPciInfo`.
type PciInfo = unsafe extern "C" fn(*mut c_void, *mut NvmlPciInfo) -> c_int;
/// `nvmlDeviceGetName`.
type DeviceName = unsafe extern "C" fn(*mut c_void, *mut c_char, c_uint) -> c_int;
/// `nvmlSystemGetDriverVersion`.
type DriverVersion = unsafe extern "C" fn(*mut c_char, c_uint) -> c_int;
/// `nvmlDeviceGetUtilizationRates`.
type Utilization = unsafe extern "C" fn(*mut c_void, *mut NvmlUtilization) -> c_int;
/// `nvmlDeviceGetMemoryInfo`.
type MemoryInfo = unsafe extern "C" fn(*mut c_void, *mut NvmlMemory) -> c_int;
/// `nvmlDeviceGetTemperature`.
type Temperature = unsafe extern "C" fn(*mut c_void, c_uint, *mut c_uint) -> c_int;
/// `nvmlDeviceGetClockInfo` (a clock id, then the value).
type ClockInfo = unsafe extern "C" fn(*mut c_void, c_uint, *mut c_uint) -> c_int;
/// One plain `unsigned int` result: fan speed, power, PCIe link, ...
type Quantity = unsafe extern "C" fn(*mut c_void, *mut c_uint) -> c_int;
/// `nvmlDeviceGetCurrentClocksThrottleReasons`.
type ThrottleReasons = unsafe extern "C" fn(*mut c_void, *mut c_ulonglong) -> c_int;

/// Every entry point this module queries.
///
/// The optional ones are `None` on a driver that does not publish them, which
/// keeps that field unavailable instead of failing the whole adapter.
struct Library {
    device_count: DeviceCount,
    handle_by_index: HandleByIndex,
    pci_info: Option<PciInfo>,
    name: Option<DeviceName>,
    driver_version: Option<DriverVersion>,
    utilization: Option<Utilization>,
    memory: Option<MemoryInfo>,
    temperature: Option<Temperature>,
    fan_speed: Option<Quantity>,
    power_usage: Option<Quantity>,
    power_limit: Option<Quantity>,
    clock_info: Option<ClockInfo>,
    pcie_link_generation: Option<Quantity>,
    pcie_link_width: Option<Quantity>,
    throttle_reasons: Option<ThrottleReasons>,
}

/// One window's NVML answer for every adapter.
#[derive(Clone)]
pub(super) struct Sample {
    pub gpus: Vec<GpuSensors>,
    /// Whether the driver refused the queries themselves instead of answering
    /// or not supporting them; only a broker can supply those readings.
    pub denied: bool,
}

/// Whether NVML loaded and initialized.
///
/// A library that loads but cannot initialize cannot answer a single query, so
/// it reports unavailable — and, because loading is attempted exactly once,
/// that answer is permanent for the process.
pub(super) fn available() -> bool {
    library().is_some()
}

/// Every adapter NVML describes, from at most one enumeration per
/// [`CACHE_TTL`].
pub(super) fn sample() -> Sample {
    let mut guard = tm_core::sync::lock(cache());
    if let Some((cached, read_at)) = guard.as_ref()
        && read_at.elapsed() < CACHE_TTL
    {
        return cached.clone();
    }
    let fresh = enumerate();
    *guard = Some((fresh.clone(), Instant::now()));
    fresh
}

fn cache() -> &'static std::sync::Mutex<Option<(Sample, Instant)>> {
    static CACHE: OnceLock<std::sync::Mutex<Option<(Sample, Instant)>>> = OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

fn library() -> Option<&'static Library> {
    // The handle itself is dropped once the symbols are bound and the library
    // is never `dlclose`d: NVML keeps driver state that must not be torn down
    // underneath live device handles, and the mapping costs nothing for a
    // process that samples GPUs.
    static LIBRARY: OnceLock<Option<Library>> = OnceLock::new();
    LIBRARY.get_or_init(load).as_ref()
}

/// Bind NVML once. `None` is permanent for this process.
fn load() -> Option<Library> {
    let handle = open_library()?;
    // Resolve the whole ABI before initializing: a missing entry point can then
    // never leave the driver initialized behind a library this module cannot
    // use.
    // SAFETY: `handle` is a live `dlopen` handle for libnvidia-ml, whose C ABI
    // the symbol names below follow.
    let (init, library): (Init, Library) = unsafe { bind(handle) }?;
    // SAFETY: `nvmlInit_v2` takes no arguments, and `init` is the non-null entry
    // point `dlsym` returned for the library that stays loaded.
    let code = unsafe { init() };
    if code != NVML_SUCCESS {
        INIT_CODE.set(code).ok();
        tracing::debug!(code, "nvmlInit_v2 failed; NVIDIA telemetry is unavailable");
        return None;
    }
    Some(library)
}

/// `libnvidia-ml.so.1`, then the development-package symlink some
/// distributions ship instead.
const LIBRARY_NAMES: [&str; 2] = ["libnvidia-ml.so.1", "libnvidia-ml.so"];

/// A live handle for the NVML image, or `None` when the host has none.
fn open_library() -> Option<*mut c_void> {
    for name in LIBRARY_NAMES {
        let Ok(symbol) = CString::new(name) else {
            continue;
        };
        // SAFETY: `symbol` is a NUL-terminated C string naming a shared object.
        // RTLD_NOW resolves its dependencies here, so a broken library fails now
        // instead of inside the first query, and the handle is kept for the
        // process's lifetime, so every pointer `dlsym` returns stays valid.
        let handle = unsafe { libc::dlopen(symbol.as_ptr(), RTLD_NOW | libc::RTLD_LOCAL) };
        if !handle.is_null() {
            return Some(handle);
        }
        tracing::debug!(library = name, "NVML library could not be opened");
    }
    // Once per process, and at warn: without this library NVIDIA telemetry can
    // never appear here, and a self-contained musl artifact (`crt-static`) has
    // no dynamic loader at all, so `dlopen` cannot succeed there however the
    // driver is installed. The loader's own reason distinguishes "no such file"
    // from "wrong ELF class", which look like an absent GPU everywhere else.
    tracing::warn!(
        reason = %loader_error(),
        static_libc = cfg!(target_feature = "crt-static"),
        "the NVIDIA management library could not be loaded; NVIDIA telemetry stays unavailable"
    );
    None
}

/// The dynamic loader's pending error text, consumed by reading it.
fn loader_error() -> String {
    // SAFETY: `dlerror` returns either null or a NUL-terminated message owned by
    // the loader and invalidated by its next call; it is copied here before
    // anything else runs.
    let raw = unsafe { libc::dlerror() };
    if raw.is_null() {
        return String::new();
    }
    // SAFETY: non-null and NUL-terminated per the `dlerror` contract.
    unsafe { std::ffi::CStr::from_ptr(raw) }
        .to_string_lossy()
        .into_owned()
}

/// Resolve every entry point of a loaded NVML image.
///
/// # Safety
///
/// `handle` must be a live `dlopen` handle for a library with the NVML C ABI,
/// and each name below must correspond to the prototype it is bound to.
unsafe fn bind(handle: *mut c_void) -> Option<(Init, Library)> {
    // SAFETY: the caller guarantees a live NVML image and the documented
    // prototypes; every bound type is an `unsafe extern "C" fn` of the NVML
    // header's signature for that name.
    unsafe {
        let init: Init = resolve(handle, "nvmlInit_v2")?;
        // The init/shutdown pair is part of the ABI probe, but `nvmlShutdown` is
        // deliberately never called: NVML stays initialized for the process's
        // whole life.
        let _shutdown: Init = resolve(handle, "nvmlShutdown")?;
        let library = Library {
            device_count: resolve(handle, "nvmlDeviceGetCount_v2")?,
            handle_by_index: resolve(handle, "nvmlDeviceGetHandleByIndex_v2")?,
            // v3 is the modern PCI-info layout; older drivers publish only the
            // legacy name, and both fill the same struct.
            pci_info: resolve(handle, "nvmlDeviceGetPciInfo_v3")
                .or_else(|| resolve(handle, "nvmlDeviceGetPciInfo")),
            name: resolve(handle, "nvmlDeviceGetName"),
            driver_version: resolve(handle, "nvmlSystemGetDriverVersion"),
            utilization: resolve(handle, "nvmlDeviceGetUtilizationRates"),
            memory: resolve(handle, "nvmlDeviceGetMemoryInfo"),
            temperature: resolve(handle, "nvmlDeviceGetTemperature"),
            fan_speed: resolve(handle, "nvmlDeviceGetFanSpeed"),
            power_usage: resolve(handle, "nvmlDeviceGetPowerUsage"),
            power_limit: resolve(handle, "nvmlDeviceGetEnforcedPowerLimit"),
            clock_info: resolve(handle, "nvmlDeviceGetClockInfo"),
            pcie_link_generation: resolve(handle, "nvmlDeviceGetCurrPcieLinkGeneration"),
            pcie_link_width: resolve(handle, "nvmlDeviceGetCurrPcieLinkWidth"),
            throttle_reasons: resolve(handle, "nvmlDeviceGetCurrentClocksThrottleReasons"),
        };
        Some((init, library))
    }
}

/// Resolve one `nvml…` entry point into the prototype its caller declared.
///
/// # Safety
///
/// `handle` must be a live `dlopen` handle, and `T` must be the
/// `unsafe extern "C" fn` type of that symbol's C signature: `dlsym` returns an
/// untyped address, so nothing checks the binding.
unsafe fn resolve<T: Copy>(handle: *mut c_void, name: &str) -> Option<T> {
    let symbol = CString::new(name).ok()?;
    // SAFETY: the caller guarantees `handle` is live, and `symbol` is a
    // NUL-terminated C string that outlives the call.
    let address = unsafe { libc::dlsym(handle, symbol.as_ptr()) };
    if address.is_null() {
        return None;
    }
    // SAFETY: the address is non-null and, per the caller's contract, points at
    // the function whose prototype `T` is; every `T` here is a function
    // pointer, i.e. exactly the pointer width `dlsym` returns.
    Some(unsafe { std::mem::transmute_copy(&address) })
}

/// Enumerate every adapter, once per call.
fn enumerate() -> Sample {
    let Some(library) = library() else {
        return Sample {
            gpus: Vec::new(),
            denied: load_denied(),
        };
    };
    let mut denied = false;
    let count = field(&mut denied, |out: &mut c_uint| unsafe {
        (library.device_count)(out)
    })
    .map_or(0, |count| count.min(MAX_DEVICES));
    let driver_version = library.driver_version.and_then(|get| {
        read_text::<DRIVER_VERSION_BUFFER>(&mut denied, |buffer, length| unsafe {
            get(buffer, length)
        })
    });
    let mut gpus = Vec::with_capacity(count as usize);
    for index in 0..count {
        if let Some(gpu) = read_device(library, index, driver_version.as_deref(), &mut denied) {
            gpus.push(gpu);
        }
    }
    Sample { gpus, denied }
}

/// Every queryable field of one adapter.
fn read_device(
    library: &Library,
    index: u32,
    driver_version: Option<&str>,
    denied: &mut bool,
) -> Option<GpuSensors> {
    let mut handle: *mut c_void = std::ptr::null_mut();
    // SAFETY: `handle_by_index` takes the index the driver itself just reported
    // and writes one device handle.
    let code = unsafe { (library.handle_by_index)(index, &mut handle) };
    if code != NVML_SUCCESS || handle.is_null() {
        // A device that fell off the bus, or a vGPU this token may not address,
        // is simply not in this snapshot.
        note(denied, code);
        return None;
    }
    let mut gpu = GpuSensors {
        index,
        driver_version: driver_version.map(str::to_string),
        ..GpuSensors::default()
    };
    if let Some(get) = library.pci_info
        && let Some(info) = field(denied, |out: &mut NvmlPciInfo| unsafe { get(handle, out) })
    {
        gpu.pci_slot = info.slot();
    }
    if let Some(get) = library.name {
        gpu.name = read_text::<NAME_BUFFER>(denied, |buffer, length| unsafe {
            get(handle, buffer, length)
        });
    }
    if let Some(get) = library.utilization
        && let Some(usage) = field(denied, |out: &mut NvmlUtilization| unsafe {
            get(handle, out)
        })
    {
        gpu.utilization_pct = nvml_percent(usage.gpu);
        gpu.memory_utilization_pct = nvml_percent(usage.memory);
    }
    if let Some(get) = library.memory
        && let Some(memory) = field(denied, |out: &mut NvmlMemory| unsafe { get(handle, out) })
    {
        // A zero total means the driver has no memory sample, and `used` is only
        // meaningful against a real total.
        if memory.total > 0 && memory.used <= memory.total {
            gpu.memory_total_bytes = Some(memory.total);
            gpu.memory_used_bytes = Some(memory.used);
        }
    }
    if let Some(get) = library.temperature {
        gpu.temperature_c = field(denied, |out: &mut c_uint| unsafe {
            get(handle, NVML_TEMPERATURE_GPU, out)
        })
        .and_then(nvml_temperature_c);
        gpu.memory_temperature_c = field(denied, |out: &mut c_uint| unsafe {
            get(handle, NVML_TEMPERATURE_MEMORY, out)
        })
        .and_then(nvml_temperature_c);
    }
    if let Some(get) = library.fan_speed {
        gpu.fan_pct =
            field(denied, |out: &mut c_uint| unsafe { get(handle, out) }).and_then(nvml_percent);
    }
    if let Some(get) = library.power_usage {
        gpu.power_w = field(denied, |out: &mut c_uint| unsafe { get(handle, out) })
            .and_then(nvml_milliwatts_to_watts);
    }
    if let Some(get) = library.power_limit {
        gpu.power_limit_w = field(denied, |out: &mut c_uint| unsafe { get(handle, out) })
            .and_then(nvml_milliwatts_to_watts);
    }
    if let Some(get) = library.clock_info {
        gpu.graphics_clock_mhz = field(denied, |out: &mut c_uint| unsafe {
            get(handle, NVML_CLOCK_GRAPHICS, out)
        })
        .and_then(nvml_clock_mhz);
        gpu.memory_clock_mhz = field(denied, |out: &mut c_uint| unsafe {
            get(handle, NVML_CLOCK_MEM, out)
        })
        .and_then(nvml_clock_mhz);
    }
    if let Some(get) = library.pcie_link_generation {
        gpu.pcie_link_generation = field(denied, |out: &mut c_uint| unsafe { get(handle, out) })
            .and_then(|generation| nvml_bounded(generation, 16));
    }
    if let Some(get) = library.pcie_link_width {
        gpu.pcie_link_width = field(denied, |out: &mut c_uint| unsafe { get(handle, out) })
            .and_then(|width| nvml_bounded(width, 32));
    }
    if let Some(get) = library.throttle_reasons {
        // `c_ulonglong` is exactly the `u64` the model carries, so the raw mask
        // needs no conversion.
        gpu.throttle_reasons = field(denied, |out: &mut c_ulonglong| unsafe { get(handle, out) });
    }
    Some(gpu)
}

/// One driver query, or `None` when the driver did not answer it.
///
/// `NVML_ERROR_NOT_SUPPORTED` is an ordinary answer for an adapter without that
/// sensor; only `NVML_ERROR_NO_PERMISSION` means a broker could do better.
fn field<T: Default>(denied: &mut bool, call: impl FnOnce(&mut T) -> c_int) -> Option<T> {
    let mut value = T::default();
    let code = call(&mut value);
    if code == NVML_SUCCESS {
        return Some(value);
    }
    note(denied, code);
    None
}

/// Remember a permission refusal, so the brokered fallback can be asked for.
fn note(denied: &mut bool, code: c_int) {
    match code {
        // An answered query, and an adapter without that sensor, are both
        // ordinary: nothing to remember.
        NVML_SUCCESS | NVML_ERROR_NOT_SUPPORTED => {}
        NVML_ERROR_NO_PERMISSION => *denied = true,
        // Anything else is unusual enough to name: a GPU that fell off the bus,
        // a driver that lost the device.
        other => tracing::trace!(code = other, "NVML query failed"),
    }
}

/// Read one NUL-terminated driver string into a fixed-size buffer.
fn read_text<const N: usize>(
    denied: &mut bool,
    call: impl FnOnce(*mut c_char, c_uint) -> c_int,
) -> Option<String> {
    let mut buffer = [0u8; N];
    let code = call(buffer.as_mut_ptr().cast::<c_char>(), N as c_uint);
    if code != NVML_SUCCESS {
        note(denied, code);
        return None;
    }
    nvml_text(&buffer)
}

/// Whether loading failed because this token may not talk to the driver.
///
/// A permanent failure keeps its reason: `NO_PERMISSION` from `nvmlInit_v2`, or
/// a control device that exists but cannot be opened, is exactly what a root
/// broker can supply. A host without the NVIDIA kernel interface has nothing to
/// ask for, so it is not "denied" and no request is ever made.
fn load_denied() -> bool {
    static DENIED: OnceLock<bool> = OnceLock::new();
    *DENIED.get_or_init(|| {
        if INIT_CODE.get().copied() == Some(NVML_ERROR_NO_PERMISSION) {
            return true;
        }
        control_device_denied()
    })
}

/// Whether the control device exists but this token may not open it.
///
/// Opening `/dev/nvidiactl` without an ioctl does not change driver state; it is
/// a permission probe only. EACCES from the mode and EPERM from a device cgroup
/// both surface as `PermissionDenied`.
fn control_device_denied() -> bool {
    std::fs::File::open(CONTROL_DEVICE)
        .is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
}
