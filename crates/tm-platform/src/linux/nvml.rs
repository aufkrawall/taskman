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
/// `nvmlDeviceGetGraphicsRunningProcesses_v3` / `…ComputeRunningProcesses_v3`.
type RunningProcesses =
    unsafe extern "C" fn(*mut c_void, *mut c_uint, *mut NvmlProcessInfo) -> c_int;
/// `nvmlDeviceGetProcessUtilization`.
type ProcessUtilization = unsafe extern "C" fn(
    *mut c_void,
    *mut NvmlProcessUtilizationSample,
    *mut c_uint,
    c_ulonglong,
) -> c_int;
/// `nvmlDeviceGetEncoderUtilization` / `…DecoderUtilization` (value, period).
type CodecUtilization = unsafe extern "C" fn(*mut c_void, *mut c_uint, *mut c_uint) -> c_int;

/// `nvmlProcessInfo_t` (= `nvmlProcessInfo_v2_t`, the `_v3` query's element):
/// `unsigned int pid; unsigned long long usedGpuMemory; unsigned int
/// gpuInstanceId; unsigned int computeInstanceId;` — 24 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct NvmlProcessInfo {
    pid: c_uint,
    used_gpu_memory: c_ulonglong,
    gpu_instance_id: c_uint,
    compute_instance_id: c_uint,
}

/// `nvmlProcessUtilizationSample_t`: pid, CPU timestamp (µs), then SM,
/// frame-buffer, encoder and decoder utilization in percent — 32 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct NvmlProcessUtilizationSample {
    pid: c_uint,
    time_stamp: c_ulonglong,
    sm_util: c_uint,
    mem_util: c_uint,
    enc_util: c_uint,
    dec_util: c_uint,
}

// The element strides the driver writes; a mismatch would misread every entry
// after the first, so it is pinned at compile time.
const _: () = assert!(std::mem::size_of::<NvmlProcessInfo>() == 24);
const _: () = assert!(std::mem::size_of::<NvmlProcessUtilizationSample>() == 32);

/// `NVML_ERROR_INSUFFICIENT_SIZE` / `NVML_ERROR_NOT_FOUND`.
const NVML_ERROR_INSUFFICIENT_SIZE: c_int = 7;
const NVML_ERROR_NOT_FOUND: c_int = 6;
/// `usedGpuMemory` when the driver cannot attribute memory (WDDM, some vGPU).
const NVML_VALUE_NOT_AVAILABLE: c_ulonglong = c_ulonglong::MAX;
/// More processes than this on one GPU is not a desktop; the bound keeps a
/// misbehaving driver from making the buffer unbounded.
const MAX_PROCESSES: c_uint = 4096;

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
    graphics_processes: Option<RunningProcesses>,
    compute_processes: Option<RunningProcesses>,
    process_utilization: Option<ProcessUtilization>,
    encoder_utilization: Option<CodecUtilization>,
    decoder_utilization: Option<CodecUtilization>,
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
            graphics_processes: resolve(handle, "nvmlDeviceGetGraphicsRunningProcesses_v3"),
            compute_processes: resolve(handle, "nvmlDeviceGetComputeRunningProcesses_v3"),
            process_utilization: resolve(handle, "nvmlDeviceGetProcessUtilization"),
            encoder_utilization: resolve(handle, "nvmlDeviceGetEncoderUtilization"),
            decoder_utilization: resolve(handle, "nvmlDeviceGetDecoderUtilization"),
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

/// One process's use of one NVIDIA adapter in the last window.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ProcessUsage {
    /// Normalized PCI slot of the adapter (the DRM join key).
    pub pci_slot: String,
    pub pid: u32,
    /// SM (3D/compute), encoder and decoder utilization in percent; `None`
    /// where the driver reported no sample for that engine.
    pub sm_pct: Option<f32>,
    pub enc_pct: Option<f32>,
    pub dec_pct: Option<f32>,
    /// Framebuffer (VRAM) bytes the driver attributes to the process.
    pub vram_bytes: Option<u64>,
}

/// Adapter-wide codec engine load, for the GPU page's engine graphs.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct AdapterEngines {
    pub pci_slot: String,
    pub enc_pct: Option<f32>,
    pub dec_pct: Option<f32>,
}

/// Every process using an NVIDIA adapter, with its engine and VRAM use.
///
/// `None` when NVML is not available or the per-process queries are not
/// published by this driver; `Some(empty)` when they answered and no process
/// uses the GPU. Utilization is "since the previous call" per adapter, so the
/// window follows the sampler's tick.
pub(super) fn process_usage() -> Option<(Vec<ProcessUsage>, Vec<AdapterEngines>)> {
    let library = library()?;
    library.graphics_processes.or(library.compute_processes)?;
    let mut denied = false;
    let count = field(&mut denied, |out: &mut c_uint| unsafe {
        (library.device_count)(out)
    })?
    .min(MAX_DEVICES);
    let mut usage = Vec::new();
    let mut engines = Vec::new();
    for index in 0..count {
        let mut handle: *mut c_void = std::ptr::null_mut();
        // SAFETY: the index comes from the driver's own count.
        let code = unsafe { (library.handle_by_index)(index, &mut handle) };
        if code != NVML_SUCCESS || handle.is_null() {
            continue;
        }
        let Some(slot) = library.pci_info.and_then(|get| {
            field(&mut denied, |out: &mut NvmlPciInfo| unsafe {
                get(handle, out)
            })
            .and_then(|info| info.slot())
        }) else {
            continue;
        };
        let slot = crate::linux_sensors::normalize_pci_slot(&slot);
        let mut by_pid: std::collections::BTreeMap<u32, ProcessUsage> =
            std::collections::BTreeMap::new();
        for query in [library.graphics_processes, library.compute_processes]
            .into_iter()
            .flatten()
        {
            for info in running_processes(query, handle) {
                let entry = by_pid.entry(info.pid).or_insert_with(|| ProcessUsage {
                    pci_slot: slot.clone(),
                    pid: info.pid,
                    sm_pct: None,
                    enc_pct: None,
                    dec_pct: None,
                    vram_bytes: None,
                });
                if info.used_gpu_memory != NVML_VALUE_NOT_AVAILABLE {
                    // A process in both lists reports the same allocation
                    // twice; it is one allocation.
                    entry.vram_bytes =
                        Some(entry.vram_bytes.unwrap_or(0).max(info.used_gpu_memory));
                }
            }
        }
        if let Some(query) = library.process_utilization {
            for sample in process_utilization(query, handle, index) {
                let entry = by_pid.entry(sample.pid).or_insert_with(|| ProcessUsage {
                    pci_slot: slot.clone(),
                    pid: sample.pid,
                    sm_pct: None,
                    enc_pct: None,
                    dec_pct: None,
                    vram_bytes: None,
                });
                let max = |current: Option<f32>, value: c_uint| {
                    nvml_percent(value).map(|value| current.map_or(value, |c| c.max(value)))
                };
                entry.sm_pct = max(entry.sm_pct, sample.sm_util).or(entry.sm_pct);
                entry.enc_pct = max(entry.enc_pct, sample.enc_util).or(entry.enc_pct);
                entry.dec_pct = max(entry.dec_pct, sample.dec_util).or(entry.dec_pct);
            }
        }
        usage.extend(by_pid.into_values());
        let codec = |get: Option<CodecUtilization>| {
            let get = get?;
            let mut value: c_uint = 0;
            let mut period: c_uint = 0;
            // SAFETY: two writable `unsigned int`s for a live handle.
            let code = unsafe { get(handle, &mut value, &mut period) };
            (code == NVML_SUCCESS)
                .then(|| nvml_percent(value))
                .flatten()
        };
        engines.push(AdapterEngines {
            pci_slot: slot,
            enc_pct: codec(library.encoder_utilization),
            dec_pct: codec(library.decoder_utilization),
        });
    }
    Some((usage, engines))
}

/// One `…RunningProcesses_v3` query: size probe, then the list.
fn running_processes(query: RunningProcesses, handle: *mut c_void) -> Vec<NvmlProcessInfo> {
    let mut count: c_uint = 0;
    // SAFETY: a null buffer with count 0 is the documented size probe.
    let code = unsafe { query(handle, &mut count, std::ptr::null_mut()) };
    if code == NVML_SUCCESS || code != NVML_ERROR_INSUFFICIENT_SIZE {
        return Vec::new();
    }
    // Head room for processes that start between the probe and the read.
    let mut capacity = count.saturating_add(8).min(MAX_PROCESSES);
    let mut buffer = vec![NvmlProcessInfo::default(); capacity as usize];
    // SAFETY: `buffer` holds `capacity` elements of the driver's 24-byte
    // layout, and `capacity` tells the driver how many it may write.
    let code = unsafe { query(handle, &mut capacity, buffer.as_mut_ptr()) };
    if code != NVML_SUCCESS {
        return Vec::new();
    }
    buffer.truncate((capacity as usize).min(buffer.len()));
    buffer
}

/// The per-process utilization samples newer than this adapter's previous
/// query. `NVML_ERROR_NOT_FOUND` means no process ran on it since then: an
/// empty answer, which is a measured idle window.
fn process_utilization(
    query: ProcessUtilization,
    handle: *mut c_void,
    index: u32,
) -> Vec<NvmlProcessUtilizationSample> {
    static LAST_SEEN: OnceLock<std::sync::Mutex<std::collections::HashMap<u32, u64>>> =
        OnceLock::new();
    let last_seen = LAST_SEEN.get_or_init(Default::default);
    let since = tm_core::sync::lock(last_seen)
        .get(&index)
        .copied()
        .unwrap_or(0);
    let mut count: c_uint = 0;
    // SAFETY: a null buffer is the documented size probe.
    let code = unsafe { query(handle, std::ptr::null_mut(), &mut count, since) };
    if code != NVML_ERROR_INSUFFICIENT_SIZE && code != NVML_SUCCESS {
        if code != NVML_ERROR_NOT_FOUND {
            tracing::trace!(code, "nvmlDeviceGetProcessUtilization probe failed");
        }
        return Vec::new();
    }
    if count == 0 {
        return Vec::new();
    }
    let mut capacity = count.saturating_add(8).min(MAX_PROCESSES);
    let mut buffer = vec![NvmlProcessUtilizationSample::default(); capacity as usize];
    // SAFETY: `buffer` holds `capacity` elements of the 32-byte layout.
    let code = unsafe { query(handle, buffer.as_mut_ptr(), &mut capacity, since) };
    if code != NVML_SUCCESS {
        return Vec::new();
    }
    buffer.truncate((capacity as usize).min(buffer.len()));
    if let Some(newest) = buffer.iter().map(|sample| sample.time_stamp).max() {
        tm_core::sync::lock(last_seen).insert(index, newest.max(since));
    }
    buffer
}
