use std::sync::Arc;

/// Keep enough width/height for 8K displays and full-surface depth textures.
const GUI_MAX_TEXTURE_DIMENSION_2D: u32 = 8192;

/// D3D12 allocates its visible CBV/SRV/UAV heap from this limit at device creation.
///
/// `TaskMan`'s bounded 512-entry icon cache needs far fewer descriptors than wgpu's
/// one-million-binding default. Leaving substantial headroom keeps ordinary textures and
/// custom UI bind groups working without reserving an oversized descriptor heap up front.
const GUI_MAX_NON_SAMPLER_BINDINGS: u32 = 8192;

/// The GUI performs small, infrequent uploads rather than large GPU workloads. wgpu's
/// performance default starts device/host allocation blocks at 128/64 MiB; memory usage
/// starts them at 8/4 MiB.
fn gui_device_memory_hints() -> wgpu::MemoryHints {
    wgpu::MemoryHints::MemoryUsage
}

/// The limits to require of the device selected from `adapter_limits`.
///
/// The GUI target is 8K, but wgpu rejects a device request that asks for more than the adapter
/// offers, and a GL driver may cap the texture dimension well below 8192 — below even the GL-safe
/// base. Asking for the target there fails `request_device` and the GUI does not start at all,
/// where asking for what the adapter has gives a smaller but working surface.
fn gui_device_limits(backend: wgpu::Backend, adapter_limits: &wgpu::Limits) -> wgpu::Limits {
    let base_limits = if backend == wgpu::Backend::Gl {
        wgpu::Limits::downlevel_webgl2_defaults()
    } else {
        wgpu::Limits::default()
    };

    // The other portable limits are validation ceilings and custom callback compatibility,
    // not allocations. Keep them at wgpu's defaults while bounding the two fields above.
    wgpu::Limits {
        max_texture_dimension_2d: GUI_MAX_TEXTURE_DIMENSION_2D
            .min(adapter_limits.max_texture_dimension_2d),
        max_non_sampler_bindings: GUI_MAX_NON_SAMPLER_BINDINGS,
        ..base_limits
    }
}

/// Request dual-source blending only when the selected adapter advertises it.
///
/// Unknown adapters must keep the ordinary grayscale path: requesting a feature they do
/// not support makes device creation fail, and requesting it from an adapter that only
/// advertises it does not create the capability. This is the exact intersection.
fn requested_dual_source_blending(adapter_features: wgpu::Features) -> wgpu::Features {
    adapter_features & wgpu::Features::DUAL_SOURCE_BLENDING
}

/// A cloneable display handle for use with [`wgpu::InstanceDescriptor`].
///
/// [`wgpu::InstanceDescriptor`] stores its display handle as a non-cloneable
/// `Box<dyn WgpuHasDisplayHandle>`. This trait wraps it so it can be cloned
/// alongside the rest of the egui wgpu configuration.
///
/// Automatically implemented for all types that satisfy the bounds
/// (including [`winit::event_loop::OwnedDisplayHandle`]).
pub trait EguiDisplayHandle:
    wgpu::rwh::HasDisplayHandle + std::fmt::Debug + Send + Sync + 'static
{
    /// Clone into a `Box<dyn WgpuHasDisplayHandle>` for [`wgpu::InstanceDescriptor::display`].
    fn clone_for_wgpu(&self) -> Box<dyn wgpu::wgt::WgpuHasDisplayHandle>;

    /// Clone into a new `Box<dyn EguiDisplayHandle>`.
    fn clone_display_handle(&self) -> Box<dyn EguiDisplayHandle>;
}

impl Clone for Box<dyn EguiDisplayHandle> {
    fn clone(&self) -> Self {
        // We need to deref here, otherwise this causes infinite recursion stack overflow.
        (**self).clone_display_handle()
    }
}

impl<T> EguiDisplayHandle for T
where
    T: wgpu::rwh::HasDisplayHandle + Clone + std::fmt::Debug + Send + Sync + 'static,
{
    fn clone_for_wgpu(&self) -> Box<dyn wgpu::wgt::WgpuHasDisplayHandle> {
        Box::new(self.clone())
    }

    fn clone_display_handle(&self) -> Box<dyn EguiDisplayHandle> {
        Box::new(self.clone())
    }
}

#[derive(Clone)]
#[expect(clippy::large_enum_variant)]
pub enum WgpuSetup {
    /// Construct a wgpu setup using some predefined settings & heuristics.
    /// This is the default option. You can customize most behaviours overriding the
    /// supported backends, power preferences, and device description.
    ///
    /// By default can also be configured with various environment variables:
    /// * `WGPU_BACKEND`: `vulkan`, `dx12`, `metal`, `opengl`, `webgpu`
    /// * `WGPU_POWER_PREF`: `low`, `high` or `none`
    /// * `WGPU_TRACE`: Path to a file to output a wgpu trace file.
    ///
    /// Each instance flag also comes with an environment variable (for details see [`wgpu::InstanceFlags`]):
    /// * `WGPU_VALIDATION`: Enables validation (enabled by default in debug builds).
    /// * `WGPU_DEBUG`: Generate debug information in shaders and objects  (enabled by default in debug builds).
    /// * `WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER`: Whether wgpu should expose adapters that run on top of non-compliant adapters.
    /// * `WGPU_GPU_BASED_VALIDATION`: Enable GPU-based validation.
    CreateNew(WgpuSetupCreateNew),

    /// Run on an existing wgpu setup.
    Existing(WgpuSetupExisting),
}

impl WgpuSetup {
    /// Creates a new [`WgpuSetup::CreateNew`] with the given display handle.
    ///
    /// See [`WgpuSetupCreateNew::from_display_handle`] for details.
    pub fn from_display_handle(display_handle: impl EguiDisplayHandle) -> Self {
        Self::CreateNew(WgpuSetupCreateNew::from_display_handle(display_handle))
    }

    /// Creates a new [`WgpuSetup::CreateNew`] without a display handle.
    ///
    /// See [`WgpuSetupCreateNew::without_display_handle`] for details.
    pub fn without_display_handle() -> Self {
        Self::CreateNew(WgpuSetupCreateNew::without_display_handle())
    }
}

impl std::fmt::Debug for WgpuSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreateNew(create_new) => f
                .debug_tuple("WgpuSetup::CreateNew")
                .field(create_new)
                .finish(),
            Self::Existing { .. } => f.debug_tuple("WgpuSetup::Existing").finish(),
        }
    }
}

impl WgpuSetup {
    /// Creates a new [`wgpu::Instance`] or clones the existing one.
    ///
    /// Does *not* store the wgpu instance, so calling this repeatedly may
    /// create a new instance every time!
    pub async fn new_instance(&self) -> wgpu::Instance {
        match self {
            Self::CreateNew(create_new) => {
                #[allow(clippy::allow_attributes, unused_mut)]
                let mut backends = create_new.instance_descriptor.backends;

                // Don't try WebGPU if we're not in a secure context.
                #[cfg(target_arch = "wasm32")]
                if backends.contains(wgpu::Backends::BROWSER_WEBGPU) {
                    let is_secure_context =
                        wgpu::web_sys::window().is_some_and(|w| w.is_secure_context());
                    if !is_secure_context {
                        log::info!(
                            "WebGPU is only available in secure contexts, i.e. on HTTPS and on localhost."
                        );
                        backends.remove(wgpu::Backends::BROWSER_WEBGPU);
                    }
                }

                log::debug!("Creating wgpu instance with backends {backends:?}");
                let desc = &create_new.instance_descriptor;
                let descriptor = wgpu::InstanceDescriptor {
                    backends: desc.backends,
                    flags: desc.flags,
                    backend_options: desc.backend_options.clone(),
                    memory_budget_thresholds: desc.memory_budget_thresholds,
                    display: create_new
                        .display_handle
                        .as_ref()
                        .map(|handle| handle.clone_for_wgpu()),
                };
                wgpu::util::new_instance_with_webgpu_detection(descriptor).await
            }
            Self::Existing(existing) => existing.instance.clone(),
        }
    }
}

impl From<WgpuSetupCreateNew> for WgpuSetup {
    fn from(create_new: WgpuSetupCreateNew) -> Self {
        Self::CreateNew(create_new)
    }
}

impl From<WgpuSetupExisting> for WgpuSetup {
    fn from(existing: WgpuSetupExisting) -> Self {
        Self::Existing(existing)
    }
}

/// Method for selecting an adapter on native.
///
/// This can be used for fully custom adapter selection.
/// If available, `wgpu::Surface` is passed to allow checking for surface compatibility.
pub type NativeAdapterSelectorMethod = Arc<
    dyn Fn(&[wgpu::Adapter], Option<&wgpu::Surface<'_>>) -> Result<wgpu::Adapter, String>
        + Send
        + Sync,
>;

/// Configuration for creating a new wgpu setup.
///
/// Used for [`WgpuSetup::CreateNew`].
///
/// Prefer [`Self::from_display_handle`] when you have a display handle available.
/// Most platforms work without one, but some (e.g. Wayland with GLES, or WebGL)
/// require it, so providing one ensures maximum compatibility.
/// With winit, pass [`EventLoop::owned_display_handle`](winit::event_loop::EventLoop::owned_display_handle).
///
/// Note: The display handle is stored in [`Self::display_handle`] rather than in
/// [`Self::instance_descriptor`] so the config can be cloned
/// ([`wgpu::InstanceDescriptor`] is not `Clone`). It is injected at instance creation time.
pub struct WgpuSetupCreateNew {
    /// Descriptor for the wgpu instance.
    ///
    /// Leave [`wgpu::InstanceDescriptor::display`] as `None` — use [`Self::display_handle`]
    /// instead (injected at instance creation time).
    ///
    /// The most important field is [`wgpu::InstanceDescriptor::backends`], which controls
    /// which backends are supported (wgpu will pick one of these). For example, set it to
    /// [`wgpu::Backends::GL`] to use only WebGL. By default on web, WebGPU is preferred
    /// with WebGL as a fallback (requires the `webgl` feature of crate `wgpu`).
    pub instance_descriptor: wgpu::InstanceDescriptor,

    /// Display handle passed to wgpu at instance creation time.
    ///
    /// Required on some platforms (e.g. Wayland with GLES, WebGL); optional elsewhere.
    /// With winit, use [`winit::event_loop::OwnedDisplayHandle`].
    ///
    /// `eframe` 's winit & web integrations will attempt to fill the display handle automatically if it is left empty.
    pub display_handle: Option<Box<dyn EguiDisplayHandle>>,

    /// Power preference for the adapter if [`Self::native_adapter_selector`] is not set or targeting web.
    pub power_preference: wgpu::PowerPreference,

    /// Optional selector for native adapters.
    ///
    /// This field has no effect when targeting web!
    /// Otherwise, if set [`Self::power_preference`] is ignored and the adapter is instead selected by this method.
    /// Note that [`Self::instance_descriptor`]'s [`wgpu::InstanceDescriptor::backends`]
    /// are still used to filter the adapter enumeration in the first place.
    ///
    /// Defaults to `None`.
    pub native_adapter_selector: Option<NativeAdapterSelectorMethod>,

    /// Configuration passed on device request, given an adapter
    pub device_descriptor:
        Arc<dyn Fn(&wgpu::Adapter) -> wgpu::DeviceDescriptor<'static> + Send + Sync>,
}

impl WgpuSetupCreateNew {
    /// Creates a new configuration with the given display handle.
    ///
    /// This is the recommended constructor. Most platforms (Windows, macOS/iOS, Android, web)
    /// work fine without a display handle, but some (e.g. Wayland on Linux with GLES) require
    /// one. Providing it unconditionally ensures your app works everywhere.
    ///
    /// If you don't have a display handle available, use [`Self::without_display_handle`]
    /// instead — it will still work on the majority of platforms.
    ///
    /// With winit, pass [`EventLoop::owned_display_handle`](winit::event_loop::EventLoop::owned_display_handle).
    pub fn from_display_handle(display_handle: impl EguiDisplayHandle) -> Self {
        Self {
            display_handle: Some(Box::new(display_handle)),
            ..Self::without_display_handle()
        }
    }

    /// Creates a new configuration without a display handle.
    ///
    /// A display handle is not required for headless operation (offscreen rendering, tests,
    /// compute-only workloads). It also isn't needed on most platforms even when presenting
    /// to a window — only some configurations (e.g. Wayland on Linux with GLES) require one.
    ///
    /// If you do have a display handle available, prefer [`Self::from_display_handle`] for
    /// maximum compatibility.
    ///
    /// With winit you can obtain one via [`EventLoop::owned_display_handle`](winit::event_loop::EventLoop::owned_display_handle).
    ///
    /// `eframe` 's winit & web integrations will attempt to fill the display handle automatically if it is left empty.
    pub fn without_display_handle() -> Self {
        Self {
            instance_descriptor: wgpu::InstanceDescriptor {
                // Add GL backend, primarily because WebGPU is not stable enough yet.
                // (note however, that the GL backend needs to be opted-in via the wgpu feature flag "webgl")
                backends: wgpu::Backends::from_env()
                    .unwrap_or(wgpu::Backends::PRIMARY | wgpu::Backends::GL),
                flags: wgpu::InstanceFlags::from_build_config().with_env(),
                backend_options: wgpu::BackendOptions::from_env_or_default(),
                memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
                display: None,
            },

            display_handle: None,

            power_preference: wgpu::PowerPreference::from_env()
                .unwrap_or(wgpu::PowerPreference::HighPerformance),

            native_adapter_selector: None,

            device_descriptor: Arc::new(|adapter| wgpu::DeviceDescriptor {
                label: Some("egui wgpu device"),
                required_features: requested_dual_source_blending(adapter.features()),
                required_limits: gui_device_limits(adapter.get_info().backend, &adapter.limits()),
                memory_hints: gui_device_memory_hints(),
                ..Default::default()
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GUI_MAX_NON_SAMPLER_BINDINGS, GUI_MAX_TEXTURE_DIMENSION_2D, gui_device_limits,
        gui_device_memory_hints, requested_dual_source_blending,
    };

    #[test]
    fn gui_device_request_uses_bounded_memory_defaults() {
        assert!(matches!(
            gui_device_memory_hints(),
            wgpu::MemoryHints::MemoryUsage
        ));

        // The requirement never exceeds the adapter, whatever the backend: wgpu validates it
        // against the adapter and fails the request instead of giving a smaller device.
        for adapter_max in [1024, 2048, 4096, 8192, 16384] {
            for backend in [wgpu::Backend::Gl, wgpu::Backend::Dx12, wgpu::Backend::Vulkan] {
                let adapter = wgpu::Limits {
                    max_texture_dimension_2d: adapter_max,
                    ..wgpu::Limits::default()
                };
                let requested = gui_device_limits(backend, &adapter).max_texture_dimension_2d;
                assert!(
                    requested <= adapter_max,
                    "{backend:?} asked for {requested} px from an adapter with {adapter_max} px"
                );
                assert!(requested <= GUI_MAX_TEXTURE_DIMENSION_2D);
            }
        }

        // An adapter that offers the GUI target keeps the full requirement.
        let large_adapter = wgpu::Limits {
            max_texture_dimension_2d: GUI_MAX_TEXTURE_DIMENSION_2D * 2,
            ..wgpu::Limits::default()
        };
        for backend in [wgpu::Backend::Dx12, wgpu::Backend::Vulkan] {
            let limits = gui_device_limits(backend, &large_adapter);
            assert_eq!(
                limits.max_texture_dimension_2d,
                GUI_MAX_TEXTURE_DIMENSION_2D
            );
            assert_eq!(
                limits.max_non_sampler_bindings,
                GUI_MAX_NON_SAMPLER_BINDINGS
            );
        }

        // A GL driver below the GUI target is asked only for what it advertises, so the GUI
        // starts with a smaller working surface instead of no window at all.
        let small_gl_adapter = wgpu::Limits {
            max_texture_dimension_2d: 2048,
            ..wgpu::Limits::downlevel_webgl2_defaults()
        };
        let gl_limits = gui_device_limits(wgpu::Backend::Gl, &small_gl_adapter);
        assert_eq!(
            gl_limits.max_texture_dimension_2d,
            small_gl_adapter.max_texture_dimension_2d
        );
        assert_eq!(
            gl_limits.max_non_sampler_bindings,
            GUI_MAX_NON_SAMPLER_BINDINGS
        );
    }

    #[test]
    fn dual_source_is_requested_only_when_the_adapter_has_it() {
        let dual = wgpu::Features::DUAL_SOURCE_BLENDING;
        assert_eq!(
            requested_dual_source_blending(wgpu::Features::empty()),
            wgpu::Features::empty()
        );
        assert_eq!(requested_dual_source_blending(dual), dual);
        assert_eq!(
            requested_dual_source_blending(dual | wgpu::Features::DEPTH_CLIP_CONTROL),
            dual,
            "unrelated adapter features must not leak into the device request"
        );
    }
}

impl Clone for WgpuSetupCreateNew {
    fn clone(&self) -> Self {
        let desc = &self.instance_descriptor;
        Self {
            instance_descriptor: wgpu::InstanceDescriptor {
                backends: desc.backends,
                flags: desc.flags,
                backend_options: desc.backend_options.clone(),
                memory_budget_thresholds: desc.memory_budget_thresholds,
                display: None,
            },
            display_handle: self.display_handle.clone(),
            power_preference: self.power_preference,
            native_adapter_selector: self.native_adapter_selector.clone(),
            device_descriptor: Arc::clone(&self.device_descriptor),
        }
    }
}

impl std::fmt::Debug for WgpuSetupCreateNew {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            instance_descriptor,
            display_handle,
            power_preference,
            native_adapter_selector,
            device_descriptor: _,
        } = self;
        f.debug_struct("WgpuSetupCreateNew")
            .field("instance_descriptor", instance_descriptor)
            .field("display_handle", display_handle)
            .field("power_preference", power_preference)
            .field(
                "native_adapter_selector",
                &native_adapter_selector.is_some(),
            )
            .finish_non_exhaustive()
    }
}

/// Configuration for using an existing wgpu setup.
///
/// Used for [`WgpuSetup::Existing`].
#[derive(Clone)]
pub struct WgpuSetupExisting {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}
