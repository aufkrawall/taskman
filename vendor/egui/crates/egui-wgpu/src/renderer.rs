use std::{borrow::Cow, num::NonZeroU64, ops::Range};

use ahash::HashMap;
use bytemuck::Zeroable as _;
use epaint::{PaintCallbackInfo, Primitive, Vertex, emath::NumExt as _};

use wgpu::util::DeviceExt as _;

// Only implements Send + Sync on wasm32 in order to allow storing wgpu resources on the type map.
#[cfg(not(all(
    target_arch = "wasm32",
    not(feature = "fragile-send-sync-non-atomic-wasm"),
)))]
/// You can use this for storage when implementing [`CallbackTrait`].
pub type CallbackResources = type_map::concurrent::TypeMap;
#[cfg(all(
    target_arch = "wasm32",
    not(feature = "fragile-send-sync-non-atomic-wasm"),
))]
/// You can use this for storage when implementing [`CallbackTrait`].
pub type CallbackResources = type_map::TypeMap;

/// You can use this to do custom [`wgpu`] rendering in an egui app.
///
/// Implement [`CallbackTrait`] and call [`Callback::new_paint_callback`].
///
/// This can be turned into a [`epaint::PaintCallback`] and [`epaint::Shape`].
pub struct Callback(Box<dyn CallbackTrait>);

impl Callback {
    /// Creates a new [`epaint::PaintCallback`] from a callback trait instance.
    pub fn new_paint_callback(
        rect: epaint::emath::Rect,
        callback: impl CallbackTrait + 'static,
    ) -> epaint::PaintCallback {
        epaint::PaintCallback {
            rect,
            callback: std::sync::Arc::new(Self(Box::new(callback))),
        }
    }
}

/// A callback trait that can be used to compose an [`epaint::PaintCallback`] via [`Callback`]
/// for custom WGPU rendering.
///
/// Callbacks in [`Renderer`] are done in three steps:
/// * [`CallbackTrait::prepare`]: called for all registered callbacks before the main egui render pass.
/// * [`CallbackTrait::finish_prepare`]: called for all registered callbacks after all callbacks finished calling prepare.
/// * [`CallbackTrait::paint`]: called for all registered callbacks during the main egui render pass.
///
/// Each callback has access to an instance of [`CallbackResources`] that is stored in the [`Renderer`].
/// This can be used to store wgpu resources that need to be accessed during the [`CallbackTrait::paint`] step.
///
/// The callbacks implementing [`CallbackTrait`] itself must always be Send + Sync, but resources stored in
/// [`Renderer::callback_resources`] are not required to implement Send + Sync when building for wasm.
/// (this is because wgpu stores references to the JS heap in most of its resources which can not be shared with other threads).
///
///
/// # Command submission
///
/// ## Command Encoder
///
/// The passed-in [`wgpu::CommandEncoder`] is egui's and can be used directly to register
/// wgpu commands for simple use cases.
/// This allows reusing the same [`wgpu::CommandEncoder`] for all callbacks and egui
/// rendering itself.
///
/// ## Command Buffers
///
/// For more complicated use cases, one can also return a list of arbitrary
/// [`wgpu::CommandBuffer`]s and have complete control over how they get created and fed.
/// In particular, this gives an opportunity to parallelize command registration and
/// prevents a faulty callback from poisoning the main wgpu pipeline.
///
/// When using eframe, the main egui command buffer, as well as all user-defined
/// command buffers returned by this function, are guaranteed to all be submitted
/// at once in a single call.
///
/// Command Buffers returned by [`CallbackTrait::finish_prepare`] will always be issued *after*
/// those returned by [`CallbackTrait::prepare`].
/// Order within command buffers returned by [`CallbackTrait::prepare`] is dependent
/// on the order the respective [`epaint::Shape::Callback`]s were submitted in.
///
/// # Example
///
/// See the [`custom3d_wgpu`](https://github.com/emilk/egui/blob/main/crates/egui_demo_app/src/apps/custom3d_wgpu.rs) demo source for a detailed usage example.
pub trait CallbackTrait: Send + Sync {
    fn prepare(
        &self,
        _device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _screen_descriptor: &ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        _callback_resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        Vec::new()
    }

    /// Called after all [`CallbackTrait::prepare`] calls are done.
    fn finish_prepare(
        &self,
        _device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _egui_encoder: &mut wgpu::CommandEncoder,
        _callback_resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        Vec::new()
    }

    /// Called after all [`CallbackTrait::finish_prepare`] calls are done.
    ///
    /// It is given access to the [`wgpu::RenderPass`] so that it can issue draw commands
    /// into the same [`wgpu::RenderPass`] that is used for all other egui elements.
    fn paint(
        &self,
        info: PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &CallbackResources,
    );
}

/// Information about the screen used for rendering.
pub struct ScreenDescriptor {
    /// Size of the window in physical pixels.
    pub size_in_pixels: [u32; 2],

    /// High-DPI scale factor (pixels per point).
    pub pixels_per_point: f32,
}

impl ScreenDescriptor {
    /// size in "logical" points
    fn screen_size_in_points(&self) -> [f32; 2] {
        [
            self.size_in_pixels[0] as f32 / self.pixels_per_point,
            self.size_in_pixels[1] as f32 / self.pixels_per_point,
        ]
    }
}

/// Uniform buffer used when rendering.
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct UniformBuffer {
    screen_size_in_points: [f32; 2],
    dithering: u32,

    /// 1 to do manual filtering for more predictable kittest snapshot images.
    ///
    /// See also <https://github.com/emilk/egui/issues/5295>.
    predictable_texture_filtering: u32,

    /// `0` = grayscale/off, `1` = RGB stripes, `2` = BGR stripes.
    subpixel_order: u32,

    /// Enhanced-contrast weight from [`epaint::TextOptions::text_contrast`].
    subpixel_contrast: f32,

    /// WGSL uniform structs have a 16-byte size/alignment contract. Keep the Rust layout
    /// explicit rather than relying on a backend to accept the otherwise 24-byte struct.
    _padding: [u32; 2],
}

fn subpixel_order_code(mode: epaint::text::SubpixelMode) -> u32 {
    match mode {
        epaint::text::SubpixelMode::Off => 0,
        epaint::text::SubpixelMode::Rgb => 1,
        epaint::text::SubpixelMode::Bgr => 2,
    }
}

fn supports_subpixel_features(features: wgpu::Features) -> bool {
    features.contains(wgpu::Features::DUAL_SOURCE_BLENDING)
}

fn normalized_contrast(contrast: f32) -> f32 {
    if contrast.is_finite() {
        contrast.clamp(0.0, 1.0)
    } else {
        0.5
    }
}

/// Reference CPU formula for the contrast curve implemented by `egui_lcd.wgsl`.
#[cfg(test)]
fn enhanced_lcd_coverage(coverage: f32, contrast: f32) -> f32 {
    let coverage = coverage.clamp(0.0, 1.0);
    let contrast = normalized_contrast(contrast);
    (coverage * (1.0 + contrast * (1.0 - coverage))).clamp(0.0, 1.0)
}

fn lcd_shader_source() -> String {
    format!(
        "enable dual_source_blending;\n{}{}",
        include_str!("egui.wgsl"),
        include_str!("egui_lcd.wgsl")
    )
}

/// Parse and validate the complete LCD WGSL before asking a backend to compile it.
///
/// This is deterministic CPU-side coverage in the normal `egui-wgpu` test suite, including
/// on hosts with no dual-source adapter. It catches both fragment entry points, the
/// extension declaration, `@blend_src` interface shape and derivative use.
fn validate_lcd_shader_source(source: &str) -> Result<(), String> {
    let module =
        wgpu::naga::front::wgsl::parse_str(source).map_err(|error| error.emit_to_string(source))?;
    wgpu::naga::valid::Validator::new(
        wgpu::naga::valid::ValidationFlags::all(),
        wgpu::naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .map_err(|error| error.emit_to_string(source))?;
    Ok(())
}

/// Resolve an error scope without an unbounded wait.
///
/// wgpu may deliver validation results through a future or a device callback. One
/// non-blocking poll covers the normal immediate case; a bounded device wait covers a
/// deferred callback. If neither resolves, the caller retains the ordinary pipeline and
/// leaves LCD capability false rather than risking startup.
fn bounded_validation_error(
    device: &wgpu::Device,
    error: impl std::future::Future<Output = Option<wgpu::Error>>,
) -> Result<Option<wgpu::Error>, ()> {
    let mut error = std::pin::pin!(error);
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    match error.as_mut().poll(&mut context) {
        std::task::Poll::Ready(error) => return Ok(error),
        std::task::Poll::Pending => {}
    }
    device.poll(wgpu::PollType::Poll).map_err(|_error| ())?;
    match error.as_mut().poll(&mut context) {
        std::task::Poll::Ready(error) => return Ok(error),
        std::task::Poll::Pending => {}
    }
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(std::time::Duration::from_millis(250)),
        })
        .map_err(|_error| ())?;
    match error.as_mut().poll(&mut context) {
        std::task::Poll::Ready(error) => Ok(error),
        std::task::Poll::Pending => Err(()),
    }
}

/// Did the managed font atlas contain LCD coverage after `delta`, given the previous
/// state?
///
/// LCD texels have `alpha == 0` and at least one non-zero RGB channel. A full replacement
/// starts over because it is a newly rasterized atlas. A partial update only allocates new
/// glyph slots, so once enabled it must not rescan every unrelated later atlas delta.
fn font_subpixel_after(was: bool, delta: &epaint::ImageDelta) -> bool {
    if was && delta.pos.is_some() {
        return true;
    }
    let epaint::ImageData::Color(image) = &delta.image;
    image
        .pixels
        .iter()
        .copied()
        .any(epaint::text::subpixel::is_lcd_texel)
}

#[expect(clippy::fn_params_excessive_bools)]
fn use_lcd_pipeline(
    texture_id: epaint::TextureId,
    all_white_uv: bool,
    lcd_available: bool,
    font_atlas_has_subpixel: bool,
    mode: epaint::text::SubpixelMode,
) -> bool {
    lcd_available
        && texture_id == epaint::TextureId::default()
        && !all_white_uv
        && font_atlas_has_subpixel
        && !mode.is_off()
}

/// The shader and this helper use the same "one texel per display pixel, at its center"
/// policy. It exists as a pure function so the transform fallback stays regression-tested
/// without requiring a GPU in unit tests.
#[cfg(test)]
fn texel_mapping_is_one_to_one(
    dx: [f32; 2],
    dy: [f32; 2],
    texel_center_fraction: [f32; 2],
) -> bool {
    let error = (dx[0] - 1.0).abs() + dx[1].abs() + dy[0].abs() + (dy[1] - 1.0).abs();
    error < 0.01 && texel_center_fraction[0] < 0.02 && texel_center_fraction[1] < 0.02
}

struct SlicedBuffer {
    buffer: wgpu::Buffer,
    slices: Vec<Range<usize>>,
    capacity: wgpu::BufferAddress,
    underused_updates: u16,
}

const BUFFER_SHRINK_MIN_CAPACITY: wgpu::BufferAddress = 1024 * 1024;
const BUFFER_SHRINK_AFTER_UPDATES: u16 = 120;
const VERTEX_BUFFER_START_CAPACITY: wgpu::BufferAddress =
    (std::mem::size_of::<Vertex>() * 1024) as _;
const INDEX_BUFFER_START_CAPACITY: wgpu::BufferAddress =
    (std::mem::size_of::<u32>() * 1024 * 3) as _;

/// Return a replacement capacity after the buffer has remained mostly unused for long
/// enough to rule out alternating large/small frames.
fn capacity_after_sustained_underuse(
    capacity: wgpu::BufferAddress,
    required: wgpu::BufferAddress,
    underused_updates: &mut u16,
    initial_capacity: wgpu::BufferAddress,
) -> Option<wgpu::BufferAddress> {
    if capacity <= initial_capacity
        || capacity < BUFFER_SHRINK_MIN_CAPACITY
        || required > capacity / 4
    {
        *underused_updates = 0;
        return None;
    }

    *underused_updates = underused_updates.saturating_add(1);
    if *underused_updates < BUFFER_SHRINK_AFTER_UPDATES {
        return None;
    }

    *underused_updates = 0;
    Some(
        required
            .saturating_mul(2)
            .max(initial_capacity)
            .min(capacity),
    )
}

pub struct Texture {
    /// The texture may be None if the `TextureId` is just a handle to a user-provided bind-group.
    pub texture: Option<wgpu::Texture>,

    /// Bindgroup for the texture + sampler.
    pub bind_group: wgpu::BindGroup,

    /// Options describing the sampler used in the bind group. This may be None if the `TextureId`
    /// is just a handle to a user-provided bind-group.
    pub options: Option<epaint::textures::TextureOptions>,
}

/// Ways to configure [`Renderer`] during creation.
#[derive(Clone, Copy, Debug)]
pub struct RendererOptions {
    /// Set the level of the multisampling anti-aliasing (MSAA).
    ///
    /// Must be a power-of-two. Higher = more smooth 3D.
    ///
    /// A value of `0` or `1` turns it off (default).
    ///
    /// `egui` already performs anti-aliasing via "feathering"
    /// (controlled by [`egui::epaint::TessellationOptions`]),
    /// but if you are embedding 3D in egui you may want to turn on multisampling.
    pub msaa_samples: u32,

    /// What format to use for the depth and stencil buffers,
    /// e.g. [`wgpu::TextureFormat::Depth32FloatStencil8`].
    ///
    /// egui doesn't need depth/stencil, so the default value is `None` (no depth or stancil buffers).
    pub depth_stencil_format: Option<wgpu::TextureFormat>,

    /// Controls whether to apply dithering to minimize banding artifacts.
    ///
    /// Dithering assumes an sRGB output and thus will apply noise to any input value that lies between
    /// two 8bit values after applying the sRGB OETF function, i.e. if it's not a whole 8bit value in "gamma space".
    /// This means that only inputs from texture interpolation and vertex colors should be affected in practice.
    ///
    /// Defaults to true.
    pub dithering: bool,

    /// Perform texture filtering in software?
    ///
    /// This is useful when you want predictable rendering across
    /// different hardware, e.g. for kittest snapshots.
    ///
    /// Default is `false`.
    ///
    /// See also <https://github.com/emilk/egui/issues/5295>.
    pub predictable_texture_filtering: bool,
}

impl RendererOptions {
    /// Set options that produce the most predicatable output.
    ///
    /// Useful for image snapshot tests.
    pub const PREDICTABLE: Self = Self {
        msaa_samples: 1,
        depth_stencil_format: None,
        dithering: false,
        predictable_texture_filtering: true,
    };
}

impl Default for RendererOptions {
    fn default() -> Self {
        Self {
            msaa_samples: 0,
            depth_stencil_format: None,
            dithering: true,
            predictable_texture_filtering: false,
        }
    }
}

/// Renderer for a egui based GUI.
pub struct Renderer {
    pipeline: wgpu::RenderPipeline,

    /// Draws the managed font atlas with per-channel dual-source blending, when the
    /// device, atlas contents and active text mode all allow it.
    lcd_pipeline: Option<wgpu::RenderPipeline>,

    /// Whether the managed font atlas currently contains the zero-alpha LCD tag used by
    /// sub-pixel coverage. A full replacement recomputes this; partial glyph additions
    /// retain it or turn it on.
    font_atlas_has_subpixel: bool,

    /// Current atlas contract and weight. These are uploaded through the uniform buffer;
    /// the renderer never guesses them from colors.
    subpixel_mode: epaint::text::SubpixelMode,
    subpixel_contrast: f32,

    index_buffer: SlicedBuffer,
    vertex_buffer: SlicedBuffer,

    uniform_buffer: wgpu::Buffer,
    previous_uniform_buffer_content: UniformBuffer,
    uniform_bind_group: wgpu::BindGroup,
    texture_bind_group_layout: wgpu::BindGroupLayout,

    /// Map of egui texture IDs to textures and their associated bindgroups (texture view +
    /// sampler). The texture may be None if the `TextureId` is just a handle to a user-provided
    /// sampler.
    textures: HashMap<epaint::TextureId, Texture>,
    next_user_texture_id: u64,
    samplers: HashMap<epaint::textures::TextureOptions, wgpu::Sampler>,

    options: RendererOptions,

    /// Storage for resources shared with all invocations of [`CallbackTrait`]'s methods.
    ///
    /// See also [`CallbackTrait`].
    pub callback_resources: CallbackResources,
}

impl Renderer {
    /// Whether this device can draw sub-pixel text.
    ///
    /// This is deliberately checked on the device rather than the adapter alone: a caller
    /// may supply an existing `wgpu::Device`, and an adapter feature only matters if the
    /// application actually requested it during device creation.
    pub fn supports_subpixel_text(device: &wgpu::Device) -> bool {
        supports_subpixel_features(device.features())
    }

    /// Whether [`Self::new`] created the LCD pipeline on this renderer.
    pub fn has_subpixel_pipeline(&self) -> bool {
        self.lcd_pipeline.is_some()
    }

    /// Publish the atlas contract and the user's text-weight choice for the next frame.
    ///
    /// RGB/BGR is a blend-time swap because the atlas is always filtered in RGB signal
    /// order. Contrast is the renderer's coverage-weight curve. The software renderer's
    /// nonlinear destination-gamma blend is documented as a GPU limitation: fixed-function
    /// blending cannot reproduce it without reading the framebuffer.
    pub fn set_subpixel_text_options(&mut self, mode: epaint::text::SubpixelMode, contrast: f32) {
        self.subpixel_mode = mode;
        self.subpixel_contrast = normalized_contrast(contrast);
    }

    /// Creates a renderer for a egui UI.
    ///
    /// `output_color_format` should preferably be [`wgpu::TextureFormat::Rgba8Unorm`] or
    /// [`wgpu::TextureFormat::Bgra8Unorm`], i.e. in gamma-space.
    #[expect(clippy::single_match_else)]
    pub fn new(
        device: &wgpu::Device,
        output_color_format: wgpu::TextureFormat,
        options: RendererOptions,
    ) -> Self {
        profiling::function_scope!();

        let shader = wgpu::ShaderModuleDescriptor {
            label: Some("egui"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(include_str!("egui.wgsl"))),
        };
        let module = {
            profiling::scope!("create_shader_module");
            device.create_shader_module(shader)
        };

        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("egui_uniform_buffer"),
            contents: bytemuck::cast_slice(&[UniformBuffer {
                screen_size_in_points: [0.0, 0.0],
                dithering: u32::from(options.dithering),
                predictable_texture_filtering: u32::from(options.predictable_texture_filtering),
                subpixel_order: subpixel_order_code(epaint::text::SubpixelMode::Off),
                subpixel_contrast: 0.5,
                _padding: [0; 2],
            }]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let uniform_bind_group_layout = {
            profiling::scope!("create_bind_group_layout");
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("egui_uniform_bind_group_layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        has_dynamic_offset: false,
                        min_binding_size: NonZeroU64::new(std::mem::size_of::<UniformBuffer>() as _),
                        ty: wgpu::BufferBindingType::Uniform,
                    },
                    count: None,
                }],
            })
        };

        let uniform_bind_group = {
            profiling::scope!("create_bind_group");
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("egui_uniform_bind_group"),
                layout: &uniform_bind_group_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &uniform_buffer,
                        offset: 0,
                        size: None,
                    }),
                }],
            })
        };

        let texture_bind_group_layout = {
            profiling::scope!("create_bind_group_layout");
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("egui_texture_bind_group_layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            multisampled: false,
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            })
        };

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("egui_pipeline_layout"),
            bind_group_layouts: &[
                Some(&uniform_bind_group_layout),
                Some(&texture_bind_group_layout),
            ],
            immediate_size: 0,
        });

        let depth_stencil = options
            .depth_stencil_format
            .map(|format| wgpu::DepthStencilState {
                format,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            });

        let create_pipeline = |label: &str,
                               module: &wgpu::ShaderModule,
                               fragment_entry_point: &str,
                               blend: wgpu::BlendState| {
            profiling::scope!("create_render_pipeline");
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    entry_point: Some("vs_main"),
                    module,
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: 5 * 4,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        // 0: vec2 position
                        // 1: vec2 texture coordinates
                        // 2: uint color
                        attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Uint32],
                    })],
                    compilation_options: wgpu::PipelineCompilationOptions::default()
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    unclipped_depth: false,
                    conservative: false,
                    cull_mode: None,
                    front_face: wgpu::FrontFace::default(),
                    polygon_mode: wgpu::PolygonMode::default(),
                    strip_index_format: None,
                },
                depth_stencil: depth_stencil.clone(),
                multisample: wgpu::MultisampleState {
                    alpha_to_coverage_enabled: false,
                    count: options.msaa_samples.max(1),
                    mask: !0,
                },

                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some(fragment_entry_point),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: output_color_format,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: wgpu::PipelineCompilationOptions::default()
                }),
                multiview_mask: None,
                cache: None,
            })
        };

        let linear_framebuffer = output_color_format.is_srgb();
        if linear_framebuffer {
            log::warn!(
                "Detected a linear (sRGBA aware) framebuffer {output_color_format:?}. egui prefers Rgba8Unorm or Bgra8Unorm"
            );
        }

        let alpha_blend = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::OneMinusDstAlpha,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        };
        let pipeline = create_pipeline(
            "egui_pipeline",
            &module,
            if linear_framebuffer {
                "fs_main_linear_framebuffer"
            } else {
                "fs_main_gamma_framebuffer"
            },
            wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: alpha_blend,
            },
        );

        // The font atlas can use a source color plus a per-channel coverage source. Both
        // sources share location 0; WGSL marks the second output with `@blend_src(1)`.
        // Validate the complete shader on the CPU first, then keep backend validation in a
        // bounded error scope. Any failure drops only the LCD pipeline.
        let lcd_pipeline = if Self::supports_subpixel_text(device) {
            let source = lcd_shader_source();
            match validate_lcd_shader_source(&source) {
                Err(error) => {
                    log::warn!("LCD shader validation failed; using grayscale pipeline: {error}");
                    None
                }
                Ok(()) => {
                    let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
                    let internal = device.push_error_scope(wgpu::ErrorFilter::Internal);
                    let out_of_memory = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
                    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                        label: Some("egui_lcd"),
                        source: wgpu::ShaderSource::Wgsl(Cow::Owned(source)),
                    });
                    let pipeline = create_pipeline(
                        "egui_lcd_pipeline",
                        &module,
                        if linear_framebuffer {
                            "fs_lcd_linear_framebuffer"
                        } else {
                            "fs_lcd_gamma_framebuffer"
                        },
                        wgpu::BlendState {
                            color: wgpu::BlendComponent {
                                src_factor: wgpu::BlendFactor::One,
                                dst_factor: wgpu::BlendFactor::OneMinusSrc1,
                                operation: wgpu::BlendOperation::Add,
                            },
                            alpha: alpha_blend,
                        },
                    );
                    let mut failed = false;
                    for (kind, scope) in [
                        (
                            "out-of-memory",
                            bounded_validation_error(device, out_of_memory.pop()),
                        ),
                        ("internal", bounded_validation_error(device, internal.pop())),
                        (
                            "validation",
                            bounded_validation_error(device, validation.pop()),
                        ),
                    ] {
                        match scope {
                            Ok(None) => {}
                            Ok(Some(error)) => {
                                failed = true;
                                log::warn!(
                                    "LCD pipeline {kind} error; using grayscale pipeline: {error}"
                                );
                            }
                            Err(()) => {
                                failed = true;
                                log::warn!(
                                    "LCD pipeline {kind} validation did not complete within 250 ms; \
                                     using grayscale pipeline"
                                );
                            }
                        }
                    }
                    if failed { None } else { Some(pipeline) }
                }
            }
        } else {
            None
        };

        Self {
            pipeline,
            lcd_pipeline,
            font_atlas_has_subpixel: false,
            subpixel_mode: epaint::text::SubpixelMode::Off,
            subpixel_contrast: 0.5,
            vertex_buffer: SlicedBuffer {
                buffer: create_vertex_buffer(device, VERTEX_BUFFER_START_CAPACITY),
                slices: Vec::with_capacity(64),
                capacity: VERTEX_BUFFER_START_CAPACITY,
                underused_updates: 0,
            },
            index_buffer: SlicedBuffer {
                buffer: create_index_buffer(device, INDEX_BUFFER_START_CAPACITY),
                slices: Vec::with_capacity(64),
                capacity: INDEX_BUFFER_START_CAPACITY,
                underused_updates: 0,
            },
            uniform_buffer,
            // Buffers on wgpu are zero initialized, so this is indeed its current state!
            previous_uniform_buffer_content: UniformBuffer::zeroed(),
            uniform_bind_group,
            texture_bind_group_layout,
            textures: HashMap::default(),
            next_user_texture_id: 0,
            samplers: HashMap::default(),
            options,
            callback_resources: CallbackResources::default(),
        }
    }

    /// Executes the egui renderer onto an existing wgpu renderpass.
    ///
    /// Note that the lifetime of `render_pass` is `'static` which requires a call to [`wgpu::RenderPass::forget_lifetime`].
    /// This allows users to pass resources that live outside of the callback resources to the render pass.
    /// The render pass internally keeps all referenced resources alive as long as necessary.
    /// The only consequence of `forget_lifetime` is that any operation on the parent encoder will cause a runtime error
    /// instead of a compile time error.
    ///
    /// # Panic
    /// Always ensure that [`Renderer::update_buffers`] has been called otherwise calling [`Renderer::render`] will panic!
    pub fn render(
        &self,
        render_pass: &mut wgpu::RenderPass<'static>,
        paint_jobs: &[epaint::ClippedPrimitive],
        screen_descriptor: &ScreenDescriptor,
    ) {
        profiling::function_scope!();

        let pixels_per_point = screen_descriptor.pixels_per_point;
        let size_in_pixels = screen_descriptor.size_in_pixels;

        // Whether or not we need to reset the render pass because a paint callback has just
        // run.
        let mut needs_reset = true;
        let mut lcd_pipeline_set = false;

        let mut index_buffer_slices = self.index_buffer.slices.iter();
        let mut vertex_buffer_slices = self.vertex_buffer.slices.iter();

        for epaint::ClippedPrimitive {
            clip_rect,
            primitive,
        } in paint_jobs
        {
            if needs_reset {
                render_pass.set_viewport(
                    0.0,
                    0.0,
                    size_in_pixels[0] as f32,
                    size_in_pixels[1] as f32,
                    0.0,
                    1.0,
                );
                render_pass.set_pipeline(&self.pipeline);
                render_pass.set_bind_group(0, &self.uniform_bind_group, &[]);
                lcd_pipeline_set = false;
                needs_reset = false;
            }

            {
                let rect = ScissorRect::new(clip_rect, pixels_per_point, size_in_pixels);

                if rect.width == 0 || rect.height == 0 {
                    // Skip rendering zero-sized clip areas.
                    if let Primitive::Mesh(_) = primitive {
                        // If this is a mesh, we need to advance the index and vertex buffer iterators:
                        index_buffer_slices
                            .next()
                            .expect("You must call .update_buffers() before .render()");
                        vertex_buffer_slices
                            .next()
                            .expect("You must call .update_buffers() before .render()");
                    }
                    continue;
                }

                render_pass.set_scissor_rect(rect.x, rect.y, rect.width, rect.height);
            }

            match primitive {
                Primitive::Mesh(mesh) => {
                    let index_buffer_slice = index_buffer_slices
                        .next()
                        .expect("You must call .update_buffers() before .render()");
                    let vertex_buffer_slice = vertex_buffer_slices
                        .next()
                        .expect("You must call .update_buffers() before .render()");

                    if let Some(Texture { bind_group, .. }) = self.textures.get(&mesh.texture_id) {
                        let lcd_pipeline = self.lcd_pipeline.as_ref().filter(|_| {
                            if mesh.texture_id != epaint::TextureId::default()
                                || !self.font_atlas_has_subpixel
                                || self.subpixel_mode.is_off()
                            {
                                return false;
                            }
                            let all_white_uv = mesh
                                .vertices
                                .iter()
                                .all(|vertex| vertex.uv == epaint::WHITE_UV);
                            use_lcd_pipeline(
                                mesh.texture_id,
                                all_white_uv,
                                true,
                                self.font_atlas_has_subpixel,
                                self.subpixel_mode,
                            )
                        });
                        if lcd_pipeline.is_some() != lcd_pipeline_set {
                            render_pass.set_pipeline(lcd_pipeline.unwrap_or(&self.pipeline));
                            lcd_pipeline_set = lcd_pipeline.is_some();
                        }
                        render_pass.set_bind_group(1, bind_group, &[]);
                        render_pass.set_index_buffer(
                            self.index_buffer.buffer.slice(
                                index_buffer_slice.start as u64..index_buffer_slice.end as u64,
                            ),
                            wgpu::IndexFormat::Uint32,
                        );
                        render_pass.set_vertex_buffer(
                            0,
                            self.vertex_buffer.buffer.slice(
                                vertex_buffer_slice.start as u64..vertex_buffer_slice.end as u64,
                            ),
                        );
                        render_pass.draw_indexed(0..mesh.indices.len() as u32, 0, 0..1);
                    } else {
                        log::warn!("Missing texture: {:?}", mesh.texture_id);
                    }
                }
                Primitive::Callback(callback) => {
                    let Some(cbfn) = callback.callback.downcast_ref::<Callback>() else {
                        // We already warned in the `prepare` callback
                        continue;
                    };

                    let info = PaintCallbackInfo {
                        viewport: callback.rect,
                        clip_rect: *clip_rect,
                        pixels_per_point,
                        screen_size_px: size_in_pixels,
                    };

                    let viewport_px = info.viewport_in_pixels();
                    if viewport_px.width_px > 0 && viewport_px.height_px > 0 {
                        profiling::scope!("callback");

                        needs_reset = true;

                        // We're setting a default viewport for the render pass as a
                        // courtesy for the user, so that they don't have to think about
                        // it in the simple case where they just want to fill the whole
                        // paint area.
                        //
                        // The user still has the possibility of setting their own custom
                        // viewport during the paint callback, effectively overriding this
                        // one.
                        render_pass.set_viewport(
                            viewport_px.left_px as f32,
                            viewport_px.top_px as f32,
                            viewport_px.width_px as f32,
                            viewport_px.height_px as f32,
                            0.0,
                            1.0,
                        );

                        cbfn.0.paint(info, render_pass, &self.callback_resources);
                    }
                }
            }
        }

        render_pass.set_scissor_rect(0, 0, size_in_pixels[0], size_in_pixels[1]);
    }

    /// Should be called before [`Self::render`].
    pub fn update_texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        id: epaint::TextureId,
        image_delta: &epaint::ImageDelta,
    ) {
        profiling::function_scope!();

        let width = image_delta.image.width() as u32;
        let height = image_delta.image.height() as u32;

        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };

        let data_color32 = match &image_delta.image {
            epaint::ImageData::Color(image) => {
                assert_eq!(
                    width as usize * height as usize,
                    image.pixels.len(),
                    "Mismatch between texture size and texel count"
                );
                Cow::Borrowed(&image.pixels)
            }
        };
        let data_bytes: &[u8] = bytemuck::cast_slice(data_color32.as_slice());

        if id == epaint::TextureId::default()
            && self.lcd_pipeline.is_some()
            && (!self.font_atlas_has_subpixel || image_delta.pos.is_none())
        {
            self.font_atlas_has_subpixel =
                font_subpixel_after(self.font_atlas_has_subpixel, image_delta);
        }

        let queue_write_data_to_texture = |texture, origin| {
            profiling::scope!("write_texture");
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin,
                    aspect: wgpu::TextureAspect::All,
                },
                data_bytes,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(4 * width),
                    rows_per_image: Some(height),
                },
                size,
            );
        };

        // Use same label for all resources associated with this texture id (no point in retyping the type)
        let label_str = format!("egui_texid_{id:?}");
        let label = Some(label_str.as_str());

        let (texture, origin, bind_group) = if let Some(pos) = image_delta.pos {
            // update the existing texture
            let Texture {
                texture,
                bind_group,
                options,
            } = self
                .textures
                .remove(&id)
                .expect("Tried to update a texture that has not been allocated yet.");
            let texture = texture.expect("Tried to update user texture.");
            let options = options.expect("Tried to update user texture.");
            let origin = wgpu::Origin3d {
                x: pos[0] as u32,
                y: pos[1] as u32,
                z: 0,
            };

            (
                texture,
                origin,
                // If the TextureOptions are the same as the previous ones, we can reuse the bind group. Otherwise we
                // have to recreate it.
                if image_delta.options == options {
                    Some(bind_group)
                } else {
                    None
                },
            )
        } else {
            // allocate a new texture
            let texture = {
                profiling::scope!("create_texture");
                device.create_texture(&wgpu::TextureDescriptor {
                    label,
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[wgpu::TextureFormat::Rgba8Unorm],
                })
            };
            let origin = wgpu::Origin3d::ZERO;
            (texture, origin, None)
        };

        let bind_group = bind_group.unwrap_or_else(|| {
            let sampler = self
                .samplers
                .entry(image_delta.options)
                .or_insert_with(|| create_sampler(image_delta.options, device));
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label,
                layout: &self.texture_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(
                            &texture.create_view(&wgpu::TextureViewDescriptor::default()),
                        ),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
            })
        });

        queue_write_data_to_texture(&texture, origin);

        // A full update must (re)create the texture at exactly the delta's size,
        // or glyph UVs (normalized by the CPU atlas size) will sample the wrong rows.
        debug_assert!(
            image_delta.pos.is_some() || [texture.width(), texture.height()] == [width, height],
            "egui texture {id:?}: GPU texture is {}x{} but full delta is {width}x{height}",
            texture.width(),
            texture.height(),
        );

        self.textures.insert(
            id,
            Texture {
                texture: Some(texture),
                bind_group,
                options: Some(image_delta.options),
            },
        );
    }

    pub fn free_texture(&mut self, id: &epaint::TextureId) {
        if *id == epaint::TextureId::default() {
            self.font_atlas_has_subpixel = false;
        }
        if let Some(texture) = self.textures.remove(id).and_then(|t| t.texture) {
            texture.destroy();
        }
    }

    /// Get the WGPU texture and bind group associated to a texture that has been allocated by egui.
    ///
    /// This could be used by custom paint hooks to render images that have been added through
    /// [`epaint::Context::load_texture`](https://docs.rs/egui/latest/egui/struct.Context.html#method.load_texture).
    pub fn texture(&self, id: &epaint::TextureId) -> Option<&Texture> {
        self.textures.get(id)
    }

    /// Registers a [`wgpu::Texture`] with a [`epaint::TextureId`].
    ///
    /// This enables the application to reference the texture inside an image ui element.
    /// This effectively enables off-screen rendering inside the egui UI. Texture must have
    /// the texture format [`wgpu::TextureFormat::Rgba8Unorm`].
    pub fn register_native_texture(
        &mut self,
        device: &wgpu::Device,
        texture: &wgpu::TextureView,
        texture_filter: wgpu::FilterMode,
    ) -> epaint::TextureId {
        self.register_native_texture_with_sampler_options(
            device,
            texture,
            wgpu::SamplerDescriptor {
                label: Some(format!("egui_user_image_{}", self.next_user_texture_id).as_str()),
                mag_filter: texture_filter,
                min_filter: texture_filter,
                ..Default::default()
            },
        )
    }

    /// Registers a [`wgpu::Texture`] with an existing [`epaint::TextureId`].
    ///
    /// This enables applications to reuse [`epaint::TextureId`]s.
    pub fn update_egui_texture_from_wgpu_texture(
        &mut self,
        device: &wgpu::Device,
        texture: &wgpu::TextureView,
        texture_filter: wgpu::FilterMode,
        id: epaint::TextureId,
    ) {
        self.update_egui_texture_from_wgpu_texture_with_sampler_options(
            device,
            texture,
            wgpu::SamplerDescriptor {
                label: Some(format!("egui_user_image_{}", self.next_user_texture_id).as_str()),
                mag_filter: texture_filter,
                min_filter: texture_filter,
                ..Default::default()
            },
            id,
        );
    }

    /// Registers a [`wgpu::Texture`] with a [`epaint::TextureId`] while also accepting custom
    /// [`wgpu::SamplerDescriptor`] options.
    ///
    /// This allows applications to specify individual minification/magnification filters as well as
    /// custom mipmap and tiling options.
    ///
    /// The texture must have the format [`wgpu::TextureFormat::Rgba8Unorm`].
    /// Any compare function supplied in the [`wgpu::SamplerDescriptor`] will be ignored.
    #[expect(clippy::needless_pass_by_value)] // false positive
    pub fn register_native_texture_with_sampler_options(
        &mut self,
        device: &wgpu::Device,
        texture: &wgpu::TextureView,
        sampler_descriptor: wgpu::SamplerDescriptor<'_>,
    ) -> epaint::TextureId {
        profiling::function_scope!();

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            compare: None,
            ..sampler_descriptor
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(format!("egui_user_image_{}", self.next_user_texture_id).as_str()),
            layout: &self.texture_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(texture),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        let id = epaint::TextureId::User(self.next_user_texture_id);
        self.textures.insert(
            id,
            Texture {
                texture: None,
                bind_group,
                options: None,
            },
        );
        self.next_user_texture_id += 1;

        id
    }

    /// Registers a [`wgpu::Texture`] with an existing [`epaint::TextureId`] while also accepting custom
    /// [`wgpu::SamplerDescriptor`] options.
    ///
    /// This allows applications to reuse [`epaint::TextureId`]s created with custom sampler options.
    #[expect(clippy::needless_pass_by_value)] // false positive
    pub fn update_egui_texture_from_wgpu_texture_with_sampler_options(
        &mut self,
        device: &wgpu::Device,
        texture: &wgpu::TextureView,
        sampler_descriptor: wgpu::SamplerDescriptor<'_>,
        id: epaint::TextureId,
    ) {
        profiling::function_scope!();

        let Texture {
            bind_group: user_texture_binding,
            ..
        } = self
            .textures
            .get_mut(&id)
            .expect("Tried to update a texture that has not been allocated yet.");

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            compare: None,
            ..sampler_descriptor
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(format!("egui_user_image_{}", self.next_user_texture_id).as_str()),
            layout: &self.texture_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(texture),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        *user_texture_binding = bind_group;
    }

    /// Uploads the uniform, vertex and index data used by the renderer.
    /// Should be called before [`Self::render`].
    ///
    /// Returns all user-defined command buffers gathered from [`CallbackTrait::prepare`] & [`CallbackTrait::finish_prepare`] callbacks.
    pub fn update_buffers(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        paint_jobs: &[epaint::ClippedPrimitive],
        screen_descriptor: &ScreenDescriptor,
    ) -> Vec<wgpu::CommandBuffer> {
        profiling::function_scope!();

        let screen_size_in_points = screen_descriptor.screen_size_in_points();

        let uniform_buffer_content = UniformBuffer {
            screen_size_in_points,
            dithering: u32::from(self.options.dithering),
            predictable_texture_filtering: u32::from(self.options.predictable_texture_filtering),
            subpixel_order: subpixel_order_code(self.subpixel_mode),
            subpixel_contrast: self.subpixel_contrast,
            _padding: [0; 2],
        };
        if uniform_buffer_content != self.previous_uniform_buffer_content {
            profiling::scope!("update uniforms");
            queue.write_buffer(
                &self.uniform_buffer,
                0,
                bytemuck::cast_slice(&[uniform_buffer_content]),
            );
            self.previous_uniform_buffer_content = uniform_buffer_content;
        }

        // Determine how many vertices & indices need to be rendered, and gather prepare callbacks
        let mut callbacks = Vec::new();
        let (vertex_count, index_count) = {
            profiling::scope!("count_vertices_indices");
            paint_jobs.iter().fold((0, 0), |acc, clipped_primitive| {
                match &clipped_primitive.primitive {
                    Primitive::Mesh(mesh) => {
                        (acc.0 + mesh.vertices.len(), acc.1 + mesh.indices.len())
                    }
                    Primitive::Callback(callback) => {
                        if let Some(c) = callback.callback.downcast_ref::<Callback>() {
                            callbacks.push(c.0.as_ref());
                        } else {
                            log::warn!("Unknown paint callback: expected `egui_wgpu::Callback`");
                        }
                        acc
                    }
                }
            })
        };

        self.index_buffer.slices.clear();
        if index_count > 0 {
            profiling::scope!("indices", index_count.to_string().as_str());

            let required_index_buffer_size = (std::mem::size_of::<u32>() * index_count) as u64;
            if self.index_buffer.capacity < required_index_buffer_size {
                // Resize index buffer if needed.
                self.index_buffer.underused_updates = 0;
                self.index_buffer.capacity =
                    (self.index_buffer.capacity * 2).at_least(required_index_buffer_size);
                self.index_buffer.buffer = create_index_buffer(device, self.index_buffer.capacity);
            } else if let Some(capacity) = capacity_after_sustained_underuse(
                self.index_buffer.capacity,
                required_index_buffer_size,
                &mut self.index_buffer.underused_updates,
                INDEX_BUFFER_START_CAPACITY,
            ) {
                self.index_buffer.capacity = capacity;
                self.index_buffer.buffer = create_index_buffer(device, capacity);
            }

            let index_buffer_staging = queue.write_buffer_with(
                &self.index_buffer.buffer,
                0,
                #[expect(clippy::unwrap_used)] // Checked above
                NonZeroU64::new(required_index_buffer_size).unwrap(),
            );

            let Some(mut index_buffer_staging) = index_buffer_staging else {
                panic!(
                    "Failed to create staging buffer for index data. Index count: {index_count}. Required index buffer size: {required_index_buffer_size}. Actual size {} and capacity: {} (bytes)",
                    self.index_buffer.buffer.size(),
                    self.index_buffer.capacity
                );
            };

            let mut index_offset = 0;
            for epaint::ClippedPrimitive { primitive, .. } in paint_jobs {
                match primitive {
                    Primitive::Mesh(mesh) => {
                        let size = mesh.indices.len() * std::mem::size_of::<u32>();
                        let slice = index_offset..(size + index_offset);
                        index_buffer_staging
                            .slice(slice.clone())
                            .copy_from_slice(bytemuck::cast_slice(&mesh.indices));
                        self.index_buffer.slices.push(slice);
                        index_offset += size;
                    }
                    Primitive::Callback(_) => {}
                }
            }
        } else {
            if let Some(capacity) = capacity_after_sustained_underuse(
                self.index_buffer.capacity,
                0,
                &mut self.index_buffer.underused_updates,
                INDEX_BUFFER_START_CAPACITY,
            ) {
                self.index_buffer.capacity = capacity;
                self.index_buffer.buffer = create_index_buffer(device, capacity);
            }
        }
        self.vertex_buffer.slices.clear();
        if vertex_count > 0 {
            profiling::scope!("vertices", vertex_count.to_string().as_str());

            let required_vertex_buffer_size = (std::mem::size_of::<Vertex>() * vertex_count) as u64;
            if self.vertex_buffer.capacity < required_vertex_buffer_size {
                // Resize vertex buffer if needed.
                self.vertex_buffer.underused_updates = 0;
                self.vertex_buffer.capacity =
                    (self.vertex_buffer.capacity * 2).at_least(required_vertex_buffer_size);
                self.vertex_buffer.buffer =
                    create_vertex_buffer(device, self.vertex_buffer.capacity);
            } else if let Some(capacity) = capacity_after_sustained_underuse(
                self.vertex_buffer.capacity,
                required_vertex_buffer_size,
                &mut self.vertex_buffer.underused_updates,
                VERTEX_BUFFER_START_CAPACITY,
            ) {
                self.vertex_buffer.capacity = capacity;
                self.vertex_buffer.buffer = create_vertex_buffer(device, capacity);
            }

            let vertex_buffer_staging = queue.write_buffer_with(
                &self.vertex_buffer.buffer,
                0,
                #[expect(clippy::unwrap_used)] // Checked above
                NonZeroU64::new(required_vertex_buffer_size).unwrap(),
            );

            let Some(mut vertex_buffer_staging) = vertex_buffer_staging else {
                panic!(
                    "Failed to create staging buffer for vertex data. Vertex count: {vertex_count}. Required vertex buffer size: {required_vertex_buffer_size}. Actual size {} and capacity: {} (bytes)",
                    self.vertex_buffer.buffer.size(),
                    self.vertex_buffer.capacity
                );
            };

            let mut vertex_offset = 0;
            for epaint::ClippedPrimitive { primitive, .. } in paint_jobs {
                match primitive {
                    Primitive::Mesh(mesh) => {
                        let size = mesh.vertices.len() * std::mem::size_of::<Vertex>();
                        let slice = vertex_offset..(size + vertex_offset);
                        vertex_buffer_staging
                            .slice(slice.clone())
                            .copy_from_slice(bytemuck::cast_slice(&mesh.vertices));
                        self.vertex_buffer.slices.push(slice);
                        vertex_offset += size;
                    }
                    Primitive::Callback(_) => {}
                }
            }
        } else {
            if let Some(capacity) = capacity_after_sustained_underuse(
                self.vertex_buffer.capacity,
                0,
                &mut self.vertex_buffer.underused_updates,
                VERTEX_BUFFER_START_CAPACITY,
            ) {
                self.vertex_buffer.capacity = capacity;
                self.vertex_buffer.buffer = create_vertex_buffer(device, capacity);
            }
        }

        let mut user_cmd_bufs = Vec::new();
        {
            profiling::scope!("prepare callbacks");
            for callback in &callbacks {
                user_cmd_bufs.extend(callback.prepare(
                    device,
                    queue,
                    screen_descriptor,
                    encoder,
                    &mut self.callback_resources,
                ));
            }
        }
        {
            profiling::scope!("finish prepare callbacks");
            for callback in &callbacks {
                user_cmd_bufs.extend(callback.finish_prepare(
                    device,
                    queue,
                    encoder,
                    &mut self.callback_resources,
                ));
            }
        }

        user_cmd_bufs
    }
}

fn create_sampler(
    options: epaint::textures::TextureOptions,
    device: &wgpu::Device,
) -> wgpu::Sampler {
    let mag_filter = match options.magnification {
        epaint::textures::TextureFilter::Nearest => wgpu::FilterMode::Nearest,
        epaint::textures::TextureFilter::Linear => wgpu::FilterMode::Linear,
    };
    let min_filter = match options.minification {
        epaint::textures::TextureFilter::Nearest => wgpu::FilterMode::Nearest,
        epaint::textures::TextureFilter::Linear => wgpu::FilterMode::Linear,
    };
    let address_mode = match options.wrap_mode {
        epaint::textures::TextureWrapMode::ClampToEdge => wgpu::AddressMode::ClampToEdge,
        epaint::textures::TextureWrapMode::Repeat => wgpu::AddressMode::Repeat,
        epaint::textures::TextureWrapMode::MirroredRepeat => wgpu::AddressMode::MirrorRepeat,
    };
    device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some(&format!(
            "egui sampler (mag: {mag_filter:?}, min {min_filter:?})"
        )),
        mag_filter,
        min_filter,
        address_mode_u: address_mode,
        address_mode_v: address_mode,
        ..Default::default()
    })
}

fn create_vertex_buffer(device: &wgpu::Device, size: u64) -> wgpu::Buffer {
    profiling::function_scope!();
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("egui_vertex_buffer"),
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        size,
        mapped_at_creation: false,
    })
}

fn create_index_buffer(device: &wgpu::Device, size: u64) -> wgpu::Buffer {
    profiling::function_scope!();
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("egui_index_buffer"),
        usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
        size,
        mapped_at_creation: false,
    })
}

/// A Rect in physical pixel space, used for setting clipping rectangles.
struct ScissorRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl ScissorRect {
    fn new(clip_rect: &epaint::Rect, pixels_per_point: f32, target_size: [u32; 2]) -> Self {
        // Transform clip rect to physical pixels:
        let clip_min_x = pixels_per_point * clip_rect.min.x;
        let clip_min_y = pixels_per_point * clip_rect.min.y;
        let clip_max_x = pixels_per_point * clip_rect.max.x;
        let clip_max_y = pixels_per_point * clip_rect.max.y;

        // Round to integer:
        let clip_min_x = clip_min_x.round() as u32;
        let clip_min_y = clip_min_y.round() as u32;
        let clip_max_x = clip_max_x.round() as u32;
        let clip_max_y = clip_max_y.round() as u32;

        // Clamp:
        let clip_min_x = clip_min_x.clamp(0, target_size[0]);
        let clip_min_y = clip_min_y.clamp(0, target_size[1]);
        let clip_max_x = clip_max_x.clamp(clip_min_x, target_size[0]);
        let clip_max_y = clip_max_y.clamp(clip_min_y, target_size[1]);

        Self {
            x: clip_min_x,
            y: clip_min_y,
            width: clip_max_x - clip_min_x,
            height: clip_max_y - clip_min_y,
        }
    }
}

// Look at the feature flag for an explanation.
#[cfg(not(all(
    target_arch = "wasm32",
    not(feature = "fragile-send-sync-non-atomic-wasm"),
)))]
#[test]
fn renderer_impl_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Renderer>();
}

#[cfg(test)]
mod tests {
    use super::{
        BUFFER_SHRINK_AFTER_UPDATES, BUFFER_SHRINK_MIN_CAPACITY, UniformBuffer,
        capacity_after_sustained_underuse, enhanced_lcd_coverage, font_subpixel_after,
        lcd_shader_source, normalized_contrast, subpixel_order_code, supports_subpixel_features,
        texel_mapping_is_one_to_one, use_lcd_pipeline, validate_lcd_shader_source,
    };
    use epaint::{Color32, ColorImage, ImageDelta, textures::TextureOptions};

    fn delta(texel: Color32, pos: Option<[usize; 2]>) -> ImageDelta {
        let image = ColorImage::filled([2, 2], texel);
        match pos {
            Some(pos) => ImageDelta::partial(pos, image, TextureOptions::LINEAR),
            None => ImageDelta::full(image, TextureOptions::LINEAR),
        }
    }

    #[test]
    fn font_atlas_subpixel_state_tracks_full_and_partial_updates() {
        let grayscale = Color32::from_rgba_premultiplied(90, 90, 90, 90);
        let color_glyph = Color32::from_rgba_premultiplied(100, 30, 20, 200);
        let tinted_lcd = Color32::from_rgba_premultiplied(120, 90, 60, 0);
        let neutral_lcd = Color32::from_rgba_premultiplied(128, 128, 128, 0);
        let full_lcd = Color32::from_rgba_premultiplied(255, 255, 255, 0);

        // Partial additions can turn it on but never turn it off.
        assert!(!font_subpixel_after(false, &delta(grayscale, Some([0, 0]))));
        assert!(!font_subpixel_after(
            false,
            &delta(color_glyph, Some([0, 0]))
        ));
        assert!(!font_subpixel_after(
            false,
            &delta(Color32::TRANSPARENT, Some([0, 0]))
        ));
        assert!(font_subpixel_after(
            false,
            &delta(neutral_lcd, Some([0, 0]))
        ));
        assert!(font_subpixel_after(false, &delta(tinted_lcd, Some([0, 0]))));
        assert!(font_subpixel_after(false, &delta(full_lcd, Some([0, 0]))));
        assert!(font_subpixel_after(true, &delta(grayscale, Some([4, 0]))));
        assert!(font_subpixel_after(true, &delta(color_glyph, Some([4, 0]))));

        // A full replacement is a new atlas and always starts from its own contents.
        assert!(font_subpixel_after(false, &delta(neutral_lcd, None)));
        assert!(font_subpixel_after(false, &delta(tinted_lcd, None)));
        assert!(!font_subpixel_after(true, &delta(grayscale, None)));
        assert!(!font_subpixel_after(true, &delta(color_glyph, None)));
        assert!(!font_subpixel_after(
            true,
            &delta(Color32::TRANSPARENT, None)
        ));
    }

    #[test]
    fn dual_source_support_requires_the_device_feature() {
        assert!(!supports_subpixel_features(wgpu::Features::empty()));
        assert!(supports_subpixel_features(
            wgpu::Features::DUAL_SOURCE_BLENDING
        ));
        assert!(supports_subpixel_features(
            wgpu::Features::DUAL_SOURCE_BLENDING | wgpu::Features::DEPTH_CLIP_CONTROL
        ));
    }

    #[test]
    fn lcd_pipeline_is_only_for_default_subpixel_font_atlas() {
        let mode = epaint::text::SubpixelMode::Rgb;
        assert!(use_lcd_pipeline(
            epaint::TextureId::default(),
            false,
            true,
            true,
            mode
        ));
        assert!(
            !use_lcd_pipeline(epaint::TextureId::default(), true, true, true, mode),
            "untextured solid geometry uses WHITE_UV and must stay on the ordinary pipeline"
        );
        assert!(!use_lcd_pipeline(
            epaint::TextureId::User(7),
            false,
            true,
            true,
            mode
        ));
        assert!(!use_lcd_pipeline(
            epaint::TextureId::default(),
            false,
            false,
            true,
            mode
        ));
        assert!(!use_lcd_pipeline(
            epaint::TextureId::default(),
            false,
            true,
            false,
            mode
        ));
        assert!(!use_lcd_pipeline(
            epaint::TextureId::default(),
            false,
            true,
            true,
            epaint::text::SubpixelMode::Off
        ));
    }

    #[test]
    fn transformed_or_fractional_texel_mapping_falls_back() {
        let centered = [0.0, 0.0];
        assert!(texel_mapping_is_one_to_one(
            [1.0, 0.0],
            [0.0, 1.0],
            centered
        ));
        assert!(!texel_mapping_is_one_to_one(
            [0.98, 0.20],
            [-0.20, 0.98],
            centered
        ));
        assert!(!texel_mapping_is_one_to_one(
            [1.37, 0.0],
            [0.0, 1.37],
            centered
        ));
        assert!(!texel_mapping_is_one_to_one(
            [1.0, 0.0],
            [0.0, 1.0],
            [0.25, 0.0]
        ));
    }

    #[test]
    fn rgb_bgr_and_contrast_are_carried_as_renderer_data() {
        use epaint::text::SubpixelMode;
        assert_eq!(subpixel_order_code(SubpixelMode::Off), 0);
        assert_eq!(subpixel_order_code(SubpixelMode::Rgb), 1);
        assert_eq!(subpixel_order_code(SubpixelMode::Bgr), 2);
        assert_eq!(normalized_contrast(0.25), 0.25);
        assert_eq!(normalized_contrast(-1.0), 0.0);
        assert_eq!(normalized_contrast(2.0), 1.0);
        assert_eq!(normalized_contrast(f32::NAN), 0.5);
    }

    #[test]
    fn neutral_lcd_coverage_receives_the_contrast_curve() {
        assert_eq!(enhanced_lcd_coverage(0.0, 0.5), 0.0);
        assert_eq!(enhanced_lcd_coverage(1.0, 0.5), 1.0);
        assert_eq!(enhanced_lcd_coverage(0.5, 0.0), 0.5);
        assert!(
            enhanced_lcd_coverage(0.5, 0.5) > 0.5,
            "neutral LCD coverage must receive the same contrast curve as tinted coverage"
        );
    }

    #[test]
    fn uniform_layout_is_wgsl_compatible_and_explicitly_padded() {
        assert_eq!(
            std::mem::offset_of!(UniformBuffer, screen_size_in_points),
            0
        );
        assert_eq!(std::mem::offset_of!(UniformBuffer, dithering), 8);
        assert_eq!(
            std::mem::offset_of!(UniformBuffer, predictable_texture_filtering),
            12
        );
        assert_eq!(std::mem::offset_of!(UniformBuffer, subpixel_order), 16);
        assert_eq!(std::mem::offset_of!(UniformBuffer, subpixel_contrast), 20);
        assert_eq!(std::mem::size_of::<UniformBuffer>(), 32);
        assert!(std::mem::size_of::<UniformBuffer>().is_multiple_of(16));
    }

    #[test]
    fn buffers_shrink_only_after_sustained_low_use() {
        let initial = 1024 * 1024;
        let capacity = 16 * initial;
        let required = initial;
        let mut underused_updates = BUFFER_SHRINK_AFTER_UPDATES - 2;

        assert_eq!(
            capacity_after_sustained_underuse(capacity, required, &mut underused_updates, initial),
            None,
            "a buffer must not shrink on a single small frame"
        );
        assert_eq!(underused_updates, BUFFER_SHRINK_AFTER_UPDATES - 1);

        assert_eq!(
            capacity_after_sustained_underuse(capacity, required, &mut underused_updates, initial),
            Some(2 * required)
        );
        assert_eq!(underused_updates, 0);

        underused_updates = BUFFER_SHRINK_AFTER_UPDATES;
        assert_eq!(
            capacity_after_sustained_underuse(
                capacity,
                capacity / 4 + 1,
                &mut underused_updates,
                initial
            ),
            None,
            "a temporarily larger frame must reset the hysteresis"
        );
        assert_eq!(underused_updates, 0);

        underused_updates = BUFFER_SHRINK_AFTER_UPDATES;
        assert_eq!(
            capacity_after_sustained_underuse(
                BUFFER_SHRINK_MIN_CAPACITY / 2,
                0,
                &mut underused_updates,
                64 * 1024
            ),
            None,
            "small renderer buffers are not worth reallocating"
        );

        let mut underused_updates = BUFFER_SHRINK_AFTER_UPDATES - 1;
        assert_eq!(
            capacity_after_sustained_underuse(capacity, 0, &mut underused_updates, initial),
            Some(initial),
            "a sustained empty or callback-only update must return to the initial capacity"
        );
        assert_eq!(underused_updates, 0);
    }

    #[test]
    fn lcd_wgsl_parses_and_validates_both_fragment_entry_points() {
        let source = lcd_shader_source();
        assert!(source.starts_with("enable dual_source_blending;"));
        assert!(source.contains("@blend_src(0)"));
        assert!(source.contains("@blend_src(1)"));
        assert!(source.contains("dpdx("));
        assert!(source.contains("dpdy("));
        assert!(source.contains("fn fs_lcd_linear_framebuffer("));
        assert!(source.contains("fn fs_lcd_gamma_framebuffer("));
        validate_lcd_shader_source(&source)
            .unwrap_or_else(|error| panic!("LCD WGSL failed Naga validation:\n{error}"));
    }
}
