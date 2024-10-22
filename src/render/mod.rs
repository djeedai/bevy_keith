use std::{fmt::Write as _, num::NonZeroU64};

use bevy::{
    asset::{Asset, AssetEvent, AssetId},
    core_pipeline::core_2d::Transparent2d,
    ecs::{
        component::Component,
        entity::Entity,
        query::ROQueryItem,
        system::{
            lifetimeless::{Read, SRes},
            Commands, Query, Res, ResMut, SystemParamItem,
        },
        world::{FromWorld, World},
    },
    math::{bounding::Aabb2d, FloatOrd},
    prelude::*,
    render::{
        render_asset::RenderAssets,
        render_phase::{
            DrawFunctions, PhaseItem, PhaseItemExtraIndex, RenderCommand, RenderCommandResult,
            SetItemPipeline, TrackedRenderPass, ViewSortedRenderPhases,
        },
        render_resource::{
            BindGroup, BindGroupEntry, BindGroupLayout, BindGroupLayoutEntry, BindingResource,
            BindingType, BlendState, Buffer, BufferBinding, BufferBindingType,
            BufferInitDescriptor, BufferSize, BufferUsages, ColorTargetState, ColorWrites,
            FragmentState, FrontFace, MultisampleState, PipelineCache, PolygonMode, PrimitiveState,
            PrimitiveTopology, RenderPipelineDescriptor, SamplerBindingType, ShaderStages,
            ShaderType, SpecializedRenderPipeline, SpecializedRenderPipelines, TextureFormat,
            TextureSampleType, TextureViewDimension, VertexState,
        },
        renderer::{RenderDevice, RenderQueue},
        texture::{BevyDefault, FallbackImage, GpuImage, Image},
        view::{
            ExtractedView, Msaa, ViewUniform, ViewUniformOffset, ViewUniforms, VisibleEntities,
        },
        Extract,
    },
    utils::{tracing::enabled, HashMap},
    window::PrimaryWindow,
};

use crate::{
    canvas::{Canvas, OffsetAndCount, PackedPrimitiveIndex, Primitive, PrimitiveInfo, Tiles},
    text::CanvasTextId,
    PRIMITIVE_SHADER_HANDLE,
};

pub type DrawPrimitive = (
    SetItemPipeline,
    SetPrimitiveViewBindGroup<0>,
    SetPrimitiveBufferBindGroup<1>,
    SetPrimitiveTextureBindGroup<2>,
    DrawPrimitiveBatch,
);

pub struct SetPrimitiveViewBindGroup<const I: usize>;

impl<P: PhaseItem, const I: usize> RenderCommand<P> for SetPrimitiveViewBindGroup<I> {
    type Param = SRes<PrimitiveMeta>;
    type ViewQuery = Read<ViewUniformOffset>;
    type ItemQuery = ();

    fn render<'w>(
        _item: &P,
        view_uniform_offset: ROQueryItem<'w, Self::ViewQuery>,
        _entity: Option<()>,
        primitive_meta: SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        trace!("SetPrimitiveViewBindGroup: I={}", I);
        let view_bind_group = primitive_meta
            .into_inner()
            .view_bind_group
            .as_ref()
            .unwrap();
        pass.set_bind_group(I, view_bind_group, &[view_uniform_offset.offset]);
        RenderCommandResult::Success
    }
}

pub struct SetPrimitiveBufferBindGroup<const I: usize>;

impl<P: PhaseItem, const I: usize> RenderCommand<P> for SetPrimitiveBufferBindGroup<I> {
    type Param = SRes<PrimitiveMeta>;
    type ViewQuery = ();
    type ItemQuery = Read<PrimitiveBatch>;

    fn render<'w>(
        _item: &P,
        _view: ROQueryItem<'w, Self::ViewQuery>,
        primitive_batch: Option<ROQueryItem<'w, Self::ItemQuery>>,
        _primitive_meta: SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        let Some(primitive_batch) = primitive_batch else {
            return RenderCommandResult::Failure;
        };
        trace!(
            "SetPrimitiveBufferBindGroup: I={} canvas_entity={:?} bg={:?}",
            I,
            primitive_batch.canvas_entity,
            primitive_batch.bind_group(),
        );
        if let Some(bind_group) = primitive_batch.bind_group() {
            pass.set_bind_group(I, bind_group, &[]);
            trace!("SetPrimitiveBufferBindGroup: SUCCESS");
            RenderCommandResult::Success
        } else {
            trace!("SetPrimitiveBufferBindGroup: FAILURE (missing bind group)");
            RenderCommandResult::Failure
        }
    }
}

pub struct SetPrimitiveTextureBindGroup<const I: usize>;

impl<P: PhaseItem, const I: usize> RenderCommand<P> for SetPrimitiveTextureBindGroup<I> {
    type Param = SRes<ImageBindGroups>;
    type ViewQuery = ();
    type ItemQuery = Read<PrimitiveBatch>;

    fn render<'w>(
        _item: &P,
        _view: ROQueryItem<'w, Self::ViewQuery>,
        primitive_batch: Option<ROQueryItem<'w, Self::ItemQuery>>,
        image_bind_groups: SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        let Some(primitive_batch) = primitive_batch else {
            return RenderCommandResult::Failure;
        };
        let image_bind_groups = image_bind_groups.into_inner();
        if primitive_batch.image_handle_id != AssetId::<Image>::invalid() {
            trace!(
                "SetPrimitiveTextureBindGroup: I={} image={:?} (valid={})",
                I,
                primitive_batch.image_handle_id,
                if primitive_batch.image_handle_id != AssetId::<Image>::invalid() {
                    "true"
                } else {
                    "false"
                }
            );
            trace!("image_bind_groups:");
            for (handle, bind_group) in &image_bind_groups.values {
                trace!("+ ibg: {:?} = {:?}", handle, bind_group);
            }
            let Some(ibg) = image_bind_groups
                .values
                .get(&primitive_batch.image_handle_id)
            else {
                error!("Failed to find IBG!");
                return RenderCommandResult::Failure;
            };
            pass.set_bind_group(I, ibg, &[]);
        } else if let Some(ibg) = image_bind_groups.fallback.as_ref() {
            // We need a texture anyway, bind anything to make the shader happy
            pass.set_bind_group(I, ibg, &[]);
        } else {
            // We can't use this shader without a valid bind group
            return RenderCommandResult::Failure;
        }
        RenderCommandResult::Success
    }
}

pub struct DrawPrimitiveBatch;

impl<P: PhaseItem> RenderCommand<P> for DrawPrimitiveBatch {
    type Param = SRes<PrimitiveMeta>;
    type ViewQuery = ();
    type ItemQuery = Read<PrimitiveBatch>;

    fn render<'w>(
        _item: &P,
        _view: ROQueryItem<'w, Self::ViewQuery>,
        _primitive_batch: Option<ROQueryItem<'w, Self::ItemQuery>>,
        _primitive_meta: SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        // Draw a single fullscreen triangle, implicitly defined by its vertex IDs
        trace!("DrawPrimitiveBatch");
        pass.draw(0..3, 0..1);
        RenderCommandResult::Success
    }
}

#[derive(Debug, Clone)]
enum BatchBuffers {
    /// Batch buffers not ready.
    Invalid,
    /// Batch buffers ready but no bind group.
    /// Tuple contains offset and size in row count of the data in the buffer.
    Raw(u32, u32),
    /// Batch buffers ready and bind group created.
    Prepared(BindGroup),
}

impl Default for BatchBuffers {
    fn default() -> Self {
        Self::Invalid
    }
}

/// Batch of primitives sharing the same [`Canvas`] and rendering
/// characteristics, and which can be rendered with a single draw call.
#[derive(Component, Clone)]
pub struct PrimitiveBatch {
    /// Handle of the texture for the batch, or [`NIL_HANDLE_ID`] if not
    /// textured.
    image_handle_id: AssetId<Image>,
    /// Entity holding the [`Canvas`] component this batch is built from.
    canvas_entity: Entity,
    /// Bind group for the primitive buffer and tile buffers used by the batch.
    primitive_bind_group: BatchBuffers,
}

impl Default for PrimitiveBatch {
    fn default() -> Self {
        Self::invalid()
    }
}

impl PrimitiveBatch {
    /// Create a batch with invalid values, that will never merge with anyhing.
    ///
    /// This is typically used as an initializing placeholder when doing
    /// incremental batching.
    pub fn invalid() -> Self {
        PrimitiveBatch {
            image_handle_id: AssetId::<Image>::invalid(),
            canvas_entity: Entity::PLACEHOLDER,
            primitive_bind_group: BatchBuffers::Invalid,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.canvas_entity == Entity::PLACEHOLDER
    }

    /// Try to merge a batch into the current batch.
    ///
    /// Return `true` if the batch was merged, or `false` otherwise.
    pub fn try_merge(&mut self, other: &PrimitiveBatch) -> bool {
        if self.is_handle_compatible(other.image_handle_id)
            && self.canvas_entity == other.canvas_entity
        {
            // Overwrite in case self is invalid
            if self.image_handle_id == AssetId::invalid() {
                self.image_handle_id = other.image_handle_id;
            }
            true
        } else {
            false
        }
    }

    /// Get the bind group for the primitive buffers associated with this batch.
    ///
    /// Returns `Some` if the bind group was successfully prepared (created), or
    /// `None` otherwise.
    pub fn bind_group(&self) -> Option<&BindGroup> {
        match &self.primitive_bind_group {
            BatchBuffers::Prepared(bind_group) => Some(bind_group),
            _ => None,
        }
    }

    /// Check if the given image handle is compatible with the current batch.
    ///
    /// The handle is compatible if either the batch's own handle or the
    /// provided handle is invalid (non-textured), or they are both valid
    /// and equal.
    fn is_handle_compatible(&self, handle: AssetId<Image>) -> bool {
        // Any invalid handle means "no texture", which can be batched with any other
        // texture. Only different (valid) textures cannot be batched together.
        return handle == AssetId::invalid()
            || self.image_handle_id == AssetId::invalid()
            || self.image_handle_id == handle;
    }
}

#[derive(Default, Resource)]
pub struct PrimitiveMeta {
    view_bind_group: Option<BindGroup>,
}

/// Shader bind groups for all images currently in use by primitives.
#[derive(Default, Resource)]
pub struct ImageBindGroups {
    values: HashMap<AssetId<Image>, BindGroup>,
    fallback: Option<BindGroup>,
}

/// Rendering pipeline for [`Canvas`] primitives.
#[derive(Resource)]
pub struct PrimitivePipeline {
    /// Bind group layout for the uniform buffer containing the [`ViewUniform`]
    /// with the camera details of the current view being rendered.
    view_layout: BindGroupLayout,
    /// Bind group layout for the primitive buffer.
    prim_layout: BindGroupLayout,
    /// Bind group layout for the texture used by textured primitives.
    material_layout: BindGroupLayout,
}

impl FromWorld for PrimitivePipeline {
    fn from_world(world: &mut World) -> Self {
        let render_device = world.get_resource::<RenderDevice>().unwrap();

        let view_layout = render_device.create_bind_group_layout(
            "keith:canvas_view_layout",
            &[BindGroupLayoutEntry {
                binding: 0,
                visibility: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                ty: BindingType::Buffer {
                    ty: BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: Some(ViewUniform::min_size()),
                },
                count: None,
            }],
        );

        let prim_layout = render_device.create_bind_group_layout(
            "keith:canvas_prim_layout",
            &[
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: BufferSize::new(4_u64), // f32
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: BufferSize::new(4_u64), // u32
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 2,
                    visibility: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: BufferSize::new(8_u64), // u32 * 2
                    },
                    count: None,
                },
            ],
        );

        let material_layout = render_device.create_bind_group_layout(
            "quad_material_layout",
            &[
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Texture {
                        multisampled: false,
                        sample_type: TextureSampleType::Float { filterable: true },
                        view_dimension: TextureViewDimension::D2,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Sampler(SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        );

        PrimitivePipeline {
            view_layout,
            prim_layout,
            material_layout,
        }
    }
}

bitflags::bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    // NOTE: Apparently quadro drivers support up to 64x MSAA.
    // MSAA uses the highest 6 bits for the MSAA sample count - 1 to support up to 64x MSAA.
    pub struct PrimitivePipelineKey: u32 {
        const NONE               = 0;
        const MSAA_RESERVED_BITS = PrimitivePipelineKey::MSAA_MASK_BITS << PrimitivePipelineKey::MSAA_SHIFT_BITS;
    }
}

impl PrimitivePipelineKey {
    const MSAA_MASK_BITS: u32 = 0b111111;
    const MSAA_SHIFT_BITS: u32 = 32 - 6;

    pub fn from_msaa_samples(msaa_samples: u32) -> Self {
        assert!(msaa_samples > 0);
        let msaa_bits = ((msaa_samples - 1) & Self::MSAA_MASK_BITS) << Self::MSAA_SHIFT_BITS;
        PrimitivePipelineKey::from_bits_retain(msaa_bits)
    }

    pub fn msaa_samples(&self) -> u32 {
        ((self.bits() >> Self::MSAA_SHIFT_BITS) & Self::MSAA_MASK_BITS) + 1
    }
}

impl SpecializedRenderPipeline for PrimitivePipeline {
    type Key = PrimitivePipelineKey;

    fn specialize(&self, key: Self::Key) -> RenderPipelineDescriptor {
        RenderPipelineDescriptor {
            vertex: VertexState {
                shader: PRIMITIVE_SHADER_HANDLE,
                entry_point: "vertex".into(),
                shader_defs: vec![],
                buffers: vec![], // vertex-less rendering
            },
            fragment: Some(FragmentState {
                shader: PRIMITIVE_SHADER_HANDLE,
                shader_defs: vec![],
                entry_point: "fragment".into(),
                targets: vec![Some(ColorTargetState {
                    format: TextureFormat::bevy_default(),
                    blend: Some(BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: ColorWrites::ALL,
                })],
            }),
            layout: vec![
                self.view_layout.clone(),
                self.prim_layout.clone(),
                self.material_layout.clone(),
            ],
            primitive: PrimitiveState {
                front_face: FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: PolygonMode::Fill,
                conservative: false,
                topology: PrimitiveTopology::TriangleList,
                strip_index_format: None,
            },
            depth_stencil: None,
            multisample: MultisampleState {
                count: key.msaa_samples(),
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            label: Some("keith:primitive_pipeline".into()),
            push_constant_ranges: vec![],
        }
    }
}

/// Rendering data extracted from a single [`Canvas`] component during the
/// [`KeithSystem::ExtractPrimitives`] render set.
#[derive(Default)]
pub struct ExtractedCanvas {
    /// Global transform of the canvas.
    pub transform: GlobalTransform,
    pub screen_size: UVec2,
    pub canvas_origin: Vec2,
    /// Canvas rectangle relative to its origin.
    pub canvas_rect: Rect,
    /// Collection of primitives rendered in this canvas.
    pub primitives: Vec<Primitive>,
    storage: Option<Buffer>,
    storage_capacity: usize,
    tile_primitives_buffer: Option<Buffer>,
    tile_primitives_buffer_capacity: usize,
    offset_and_count_buffer: Option<Buffer>,
    offset_and_count_buffer_capacity: usize,
    /// Scale factor of the window where this canvas is rendered.
    pub scale_factor: f32,
    /// Extracted data for all texts in use, in local text ID order.
    pub(crate) texts: Vec<ExtractedText>,
    pub(crate) tiles: Tiles,
}

impl ExtractedCanvas {
    /// Write the CPU scratch buffer into the associated GPU storage buffer.
    pub fn write_buffers(
        &mut self,
        primitives: &[f32],
        render_device: &RenderDevice,
        render_queue: &RenderQueue,
    ) {
        trace!(
            "Writing {} primitive elements to GPU buffers",
            primitives.len(),
        );

        // Primitive buffer
        let size = primitives.len(); // FIXME - cap size to reasonable value
        let contents = bytemuck::cast_slice(&primitives[..]);
        if size > self.storage_capacity {
            // GPU buffer too small; reallocated...
            trace!(
                "Reallocate canvas_primitive_buffer: {} -> {}",
                self.storage_capacity,
                size
            );
            self.storage = Some(
                render_device.create_buffer_with_data(&BufferInitDescriptor {
                    label: Some("keith:canvas_primitive_buffer"),
                    usage: BufferUsages::COPY_DST | BufferUsages::STORAGE,
                    contents,
                }),
            );
            self.storage_capacity = size;
        } else if let Some(storage) = &self.storage {
            // Write directly to existing GPU buffer
            render_queue.write_buffer(storage, 0, contents);
        }

        // Tile primitives buffer
        let size = self.tiles.primitives.len(); // FIXME - cap size to reasonable value
        let contents = bytemuck::cast_slice(&self.tiles.primitives[..]);
        if size > self.tile_primitives_buffer_capacity {
            // GPU buffer too small; reallocated...
            trace!(
                "Reallocate canvas_tile_primitive_buffer: {} -> {}",
                self.tile_primitives_buffer_capacity,
                size
            );
            self.tile_primitives_buffer = Some(render_device.create_buffer_with_data(
                &BufferInitDescriptor {
                    label: Some("keith:canvas_tile_primitive_buffer"),
                    usage: BufferUsages::COPY_DST | BufferUsages::STORAGE,
                    contents,
                },
            ));
            self.tile_primitives_buffer_capacity = size;
        } else if let Some(tile_primitives_buffer) = &self.tile_primitives_buffer {
            // Write directly to existing GPU buffer
            render_queue.write_buffer(tile_primitives_buffer, 0, contents);
        }

        // Offset and count buffer
        let size = self.tiles.offset_and_count.len() * 2; // FIXME - cap size to reasonable value
        let contents = bytemuck::cast_slice(&self.tiles.offset_and_count[..]);
        if size > self.offset_and_count_buffer_capacity {
            // GPU buffer too small; reallocated...
            trace!(
                "Reallocate canvas_offset_and_count_buffer: {} -> {}",
                self.offset_and_count_buffer_capacity,
                size
            );
            self.offset_and_count_buffer = Some(render_device.create_buffer_with_data(
                &BufferInitDescriptor {
                    label: Some("keith:canvas_offset_and_count_buffer"),
                    usage: BufferUsages::COPY_DST | BufferUsages::STORAGE,
                    contents,
                },
            ));
            self.offset_and_count_buffer_capacity = size;
        } else if let Some(offset_and_count_buffer) = &self.offset_and_count_buffer {
            // Write directly to existing GPU buffer
            render_queue.write_buffer(offset_and_count_buffer, 0, contents);
        }
    }

    #[inline]
    pub fn binding(&self) -> Option<BindingResource> {
        self.storage.as_ref().map(|buffer| {
            BindingResource::Buffer(BufferBinding {
                buffer: &buffer,
                offset: 0,
                size: None,
            })
        })
    }

    #[inline]
    pub fn tile_primitives_binding(&self) -> Option<BindingResource> {
        self.tile_primitives_buffer.as_ref().map(|buffer| {
            BindingResource::Buffer(BufferBinding {
                buffer: &buffer,
                offset: 0,
                size: None,
            })
        })
    }

    #[inline]
    pub fn offset_and_count_binding(&self, offset: u32, size: u32) -> Option<BindingResource> {
        self.offset_and_count_buffer.as_ref().map(|buffer| {
            BindingResource::Buffer(BufferBinding {
                buffer: &buffer,
                offset: offset as u64 * 8,
                size: Some(NonZeroU64::new(size as u64 * 8).unwrap()),
            })
        })
    }
}

/// Resource attached to the render world and containing all the data extracted
/// from the various visible [`Canvas`] components.
#[derive(Default, Resource)]
pub struct ExtractedCanvases {
    /// Map from app world's entity with a [`Canvas`] component to associated
    /// render world's extracted canvas.
    pub canvases: HashMap<Entity, ExtractedCanvas>,
}

#[derive(Default, Resource)]
pub struct PrimitiveAssetEvents {
    pub images: Vec<AssetEvent<Image>>,
}

/// Clone an [`AssetEvent`] manually by unwrapping and re-wrapping it, returning
/// an event with a weak handle.
///
/// This is necessary because [`AssetEvent`] is `!Clone`.
#[inline]
fn clone_asset_event_weak<T: Asset>(event: &AssetEvent<T>) -> AssetEvent<T> {
    match event {
        AssetEvent::Added { id } => AssetEvent::Added { id: *id },
        AssetEvent::Modified { id } => AssetEvent::Modified { id: *id },
        AssetEvent::Removed { id } => AssetEvent::Removed { id: *id },
        AssetEvent::LoadedWithDependencies { id } => AssetEvent::LoadedWithDependencies { id: *id },
        AssetEvent::Unused { id } => AssetEvent::Unused { id: *id },
    }
}

/// Render app system consuming asset events for [`Image`] components to react
/// to changes to the content of primitive textures.
pub(crate) fn extract_primitive_events(
    mut events: ResMut<PrimitiveAssetEvents>,
    mut image_events: Extract<EventReader<AssetEvent<Image>>>,
) {
    // trace!("extract_primitive_events");

    let PrimitiveAssetEvents { ref mut images } = *events;

    images.clear();

    for image in image_events.read() {
        images.push(clone_asset_event_weak(image));
    }
}

#[derive(Debug, Default)]
pub(crate) struct ExtractedText {
    pub glyphs: Vec<ExtractedGlyph>,
}

#[derive(Debug)]
pub(crate) struct ExtractedGlyph {
    /// Offset of the glyph from the text origin, in physical pixels.
    pub offset: Vec2,
    /// Size of the glyph, in physical pixels.
    pub size: Vec2,
    /// Glyph color, as RGBA linear (0xAABBGGRR in little endian). Extracted
    /// from the text section's style ([`TextStyle::color`]).
    pub color: u32,
    /// Handle of the atlas texture where the glyph is stored.
    pub handle_id: AssetId<Image>,
    /// Rectangle in UV coordinates delimiting the glyph area in the atlas
    /// texture.
    pub uv_rect: bevy::math::Rect,
}

/// Render app system extracting all primitives from all [`Canvas`] components,
/// for later rendering.
///
/// # Dependent components
///
/// [`Canvas`] components require at least a [`GlobalTransform`] component
/// attached to the same entity and describing the canvas 3D transform.
///
/// An optional [`ComputedVisibility`] component can be added to that same
/// entity to dynamically control the canvas visibility. By default if absent
/// the canvas is assumed visible.
pub(crate) fn extract_primitives(
    mut extracted_canvases: ResMut<ExtractedCanvases>,
    texture_atlases: Extract<Res<Assets<TextureAtlasLayout>>>,
    q_window: Extract<Query<&Window, With<PrimaryWindow>>>,
    canvas_query: Extract<
        Query<(
            Entity,
            Option<&ViewVisibility>,
            &Camera,
            &OrthographicProjection,
            &Canvas,
            &GlobalTransform,
            &Tiles,
        )>,
    >,
) {
    trace!("extract_primitives");

    // TODO - handle multi-window
    let Ok(primary_window) = q_window.get_single() else {
        return;
    };
    let scale_factor = primary_window.scale_factor() as f32;
    let inv_scale_factor = 1. / scale_factor;
    trace!("window: scale_factor={scale_factor:?} inv_scale_factor={inv_scale_factor:?}");

    let extracted_canvases = &mut extracted_canvases.canvases;
    extracted_canvases.clear();

    for (entity, maybe_computed_visibility, camera, proj, canvas, transform, tiles) in
        canvas_query.iter()
    {
        // Skip hidden canvases. If no ComputedVisibility component is present, assume
        // visible.
        if !maybe_computed_visibility.map_or(true, |cvis| cvis.get()) {
            continue;
        }

        // Get screen size of camera
        let Some(screen_size) = camera.physical_viewport_size() else {
            continue;
        };

        // Swap render and main app primitive buffer
        // FIXME - Can't swap in Extract phase because main world is read-only; clone
        // instead
        let primitives = canvas.buffer().clone();
        trace!(
            "Canvas on Entity {:?} has {} primitives and {} text layouts, viewport_origin={:?}, viewport_area={:?}, scale_factor={}, proj.scale={}",
            entity,
            primitives.len(),
            canvas.text_layouts().len(),
            proj.viewport_origin,
            proj.area,
            scale_factor,
            proj.scale
        );
        if primitives.is_empty() {
            continue;
        }

        // Process text glyphs. This requires access to various assets on the main app,
        // so needs to be done during the extract phase.
        let mut extracted_texts: Vec<ExtractedText> = vec![];
        for text in canvas.text_layouts() {
            let text_id = CanvasTextId::from_raw(entity, text.id);
            trace!("Extracting text {:?}...", text_id);

            let Some(text_layout_info) = &text.layout_info else {
                trace!("Text layout not computed, skipping text...");
                continue;
            };

            trace!(
                "-> {} glyphs, scale_factor={}",
                text_layout_info.glyphs.len(),
                scale_factor
            );

            let mut extracted_glyphs = vec![];
            for text_glyph in &text_layout_info.glyphs {
                let color = text.sections[text_glyph.section_index]
                    .style
                    .color
                    .to_linear()
                    .as_u32();
                let atlas_layout = texture_atlases
                    .get(&text_glyph.atlas_info.texture_atlas)
                    .unwrap();
                let handle = text_glyph.atlas_info.texture.clone_weak();
                let index = text_glyph.atlas_info.glyph_index as usize;
                let uv_rect = atlas_layout.textures[index];

                trace!(
                    "glyph: position_px={:?} size_px={:?} color=0x{:x} glyph_index={:?} uv_rect={:?}",
                    text_glyph.position,
                    text_glyph.size,
                    color,
                    index,
                    uv_rect,
                );

                extracted_glyphs.push(ExtractedGlyph {
                    offset: text_glyph.position,
                    size: text_glyph.size,
                    color,
                    handle_id: handle.id(),
                    uv_rect: uv_rect.as_rect(),
                });
            }

            let index = text.id as usize;
            trace!(
                "Inserting index={} with {} glyphs into extracted texts of len={}...",
                index,
                extracted_glyphs.len(),
                extracted_texts.len(),
            );
            if index >= extracted_texts.len() {
                extracted_texts.resize_with(index + 1, Default::default);
            }
            extracted_texts[index].glyphs = extracted_glyphs;
        }

        // Save extracted canvas
        let extracted_canvas = extracted_canvases
            .entry(entity)
            .or_insert(ExtractedCanvas::default());
        extracted_canvas.transform = *transform;
        extracted_canvas.screen_size = screen_size;
        extracted_canvas.canvas_origin = -proj.area.min * scale_factor; // in physical pixels
        extracted_canvas.canvas_rect = canvas.rect();
        extracted_canvas.primitives = primitives;
        extracted_canvas.scale_factor = scale_factor;
        extracted_canvas.texts = extracted_texts;
        extracted_canvas.tiles = tiles.clone();
    }
}

/// Iterator over sub-primitives of a primitive.
pub(crate) struct SubPrimIter<'a> {
    /// The current primitive being iterated over, or `None` if the iterator
    /// reached the end of the iteration sequence.
    prim: Option<&'a Primitive>,
    /// The index of the current sub-primitive inside its parent primitive.
    index: usize,
    /// Text information for iterating over glyphs.
    texts: &'a [ExtractedText],
    /// Inverse scale factor, to convert from physical to logical coordinates.
    inv_scale_factor: f32,
}

impl<'a> SubPrimIter<'a> {
    pub fn new(prim: &'a Primitive, texts: &'a [ExtractedText], inv_scale_factor: f32) -> Self {
        Self {
            prim: Some(prim),
            index: 0,
            texts,
            inv_scale_factor,
        }
    }
}

impl<'a> Iterator for SubPrimIter<'a> {
    type Item = (AssetId<Image>, Aabb2d);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(prim) = self.prim {
            let PrimitiveInfo {
                row_count: _,
                sub_prim_count: _,
            } = prim.info(self.texts);
            match prim {
                Primitive::Text(text) => {
                    if text.id as usize >= self.texts.len() {
                        return None; // not ready
                    }
                    let extracted_text = &self.texts[text.id as usize];
                    if self.index < extracted_text.glyphs.len() {
                        let glyph = &extracted_text.glyphs[self.index];
                        let image_handle_id = glyph.handle_id;
                        // The AABB returned is in logical coordinates, but the text internally is
                        // always in physical coordinates.
                        let aabb = Aabb2d {
                            min: text.rect.min + glyph.offset * self.inv_scale_factor,
                            max: text.rect.min
                                + (glyph.offset + glyph.size) * self.inv_scale_factor,
                        };
                        self.index += 1;
                        Some((image_handle_id, aabb))
                    } else {
                        self.prim = None;
                        None
                    }
                }
                Primitive::Rect(rect) => {
                    let handle_id = if let Some(id) = rect.image {
                        id
                    } else {
                        AssetId::<Image>::invalid()
                    };
                    self.prim = None;
                    Some((handle_id, rect.aabb()))
                }
                _ => {
                    self.prim = None;
                    // Currently all other primitives are non-textured
                    Some((AssetId::<Image>::invalid(), prim.aabb()))
                }
            }
        } else {
            None
        }
    }
}

/// Format a list of values as 16 values per row, for more compact `trace!()`.
///
/// ```ignore
/// trace_list!("x = ", my_iter, " {}");
/// ```
macro_rules! trace_list {
    ($header:expr, $iter:expr, $fmt:expr) => {
        if enabled!(bevy::log::Level::TRACE) {
            let mut s = String::with_capacity(256);
            for u in $iter.chunks(16) {
                s.clear();
                s += $header;
                u.iter().fold(&mut s, |s, u| {
                    write!(s, $fmt, u).unwrap();
                    s
                });
                trace!("{}", s);
            }
        }
    };
}

pub(crate) struct PreparedPrimitive {
    /// AABB in canvas space, for tile assignment.
    pub aabb: Aabb2d,
    /// Primitive index.
    pub prim_index: PackedPrimitiveIndex,
}

pub(crate) fn prepare_primitives(
    mut commands: Commands,
    mut extracted_canvases: ResMut<ExtractedCanvases>,
    render_device: Res<RenderDevice>,
    render_queue: Res<RenderQueue>,
    mut image_bind_groups: ResMut<ImageBindGroups>,
    events: Res<PrimitiveAssetEvents>,
    mut prepared_primitives: Local<Vec<PreparedPrimitive>>,
) {
    trace!("prepare_primitives()");

    // If an Image has changed, the GpuImage has (probably) changed
    for event in &events.images {
        match event {
            AssetEvent::Added { .. } | AssetEvent::LoadedWithDependencies { .. } => None,
            AssetEvent::Modified { id }
            | AssetEvent::Removed { id }
            | AssetEvent::Unused { id } => {
                let removed = image_bind_groups.values.remove(id);
                if removed.is_some() {
                    debug!("Removed IBG for handle {:?} due to {:?}", id, event);
                }
                removed
            }
        };
    }

    let oc_align = render_device.limits().min_storage_buffer_offset_alignment;

    let extracted_canvases = &mut extracted_canvases.canvases;

    // Loop on all extracted canvases to process their primitives
    for (entity, extracted_canvas) in extracted_canvases {
        trace!(
            "Canvas on Entity {:?} has {} primitives and {} texts, tile size {:?}, canvas_origin={:?} canvas_rect={:?}",
            entity,
            extracted_canvas.primitives.len(),
            extracted_canvas.texts.len(),
            extracted_canvas.tiles.tile_size,
            extracted_canvas.canvas_origin,
            extracted_canvas.canvas_rect,
        );

        let mut primitives = vec![];

        prepared_primitives.clear();
        prepared_primitives.reserve(extracted_canvas.primitives.len());

        extracted_canvas.tiles.offset_and_count.clear();

        let canvas_translation = -extracted_canvas.canvas_rect.min;
        let inv_scale_factor = 1.0 / extracted_canvas.scale_factor;

        // Serialize primitives into a binary float32 array, to work around the fact
        // wgpu doesn't have byte arrays. And f32 being the most common type of
        // data in primitives limits the amount of bitcast in the shader.
        trace!(
            "Serialize {} primitives...",
            extracted_canvas.primitives.len()
        );
        let mut current_batch = PrimitiveBatch::invalid();
        let mut oc_offset = extracted_canvas.tiles.offset_and_count.len() as u32;
        let mut pp_offset = 0;
        for prim in &extracted_canvas.primitives {
            let base_index = primitives.len() as u32;
            let is_textured = prim.is_textured();
            let is_bordered = prim.is_bordered();
            let mut prim_index =
                PackedPrimitiveIndex::new(base_index, prim.gpu_kind(), is_textured, is_bordered);

            trace!("+ Primitive @ base_index={}", base_index);

            // Serialize the primitive
            let PrimitiveInfo {
                row_count,
                sub_prim_count,
            } = prim.info(&extracted_canvas.texts[..]);
            trace!(
                "  row_count={} sub_prim_count={}",
                row_count,
                sub_prim_count
            );
            if row_count > 0 && sub_prim_count > 0 {
                let row_count = row_count as usize;
                let sub_prim_count = sub_prim_count as usize;
                let total_row_count = row_count * sub_prim_count;

                // Reserve some (uninitialized) storage for new data
                primitives.reserve(total_row_count);
                let prim_slice = primitives.spare_capacity_mut();

                // Write primitives and indices directly into storage
                prim.write(
                    &extracted_canvas.texts[..],
                    &mut prim_slice[..total_row_count],
                    canvas_translation,
                    extracted_canvas.scale_factor,
                );

                // Apply new storage sizes once data is initialized
                let new_row_count = primitives.len() + total_row_count;
                unsafe { primitives.set_len(new_row_count) };

                trace!("New primitive elements: (+{})", total_row_count);
                trace_list!(
                    "+ f32[] =",
                    primitives[new_row_count - total_row_count..new_row_count],
                    " {}"
                );

                // Reserve storage for prepared primitives
                prepared_primitives.reserve(sub_prim_count);
            }

            // Loop on sub-primitives; Text primitives expand to one Rect primitive
            // per glyph, each of which _can_ have a separate atlas texture so potentially
            // can split the draw into a new batch.
            trace!("Batch sub-primitives...");
            let batch_iter = SubPrimIter::new(prim, &extracted_canvas.texts, inv_scale_factor);
            for (image_handle_id, mut aabb) in batch_iter {
                let new_batch = PrimitiveBatch {
                    image_handle_id,
                    canvas_entity: *entity,
                    ..default()
                };
                trace!(
                    "New Batch: canvas_entity={:?} image={:?}",
                    new_batch.canvas_entity,
                    new_batch.image_handle_id
                );

                // Convert from logical to physical coordinates
                aabb.min *= extracted_canvas.scale_factor;
                aabb.max *= extracted_canvas.scale_factor;
                aabb.min += extracted_canvas.canvas_origin;
                aabb.max += extracted_canvas.canvas_origin;

                if current_batch.try_merge(&new_batch) {
                    trace!(
                        "Merged new batch with current batch: image={:?}",
                        current_batch.image_handle_id
                    );

                    // Calculate once and save the AABB of the primitive, for tile assignment
                    // purpose. Since there are many more tiles than primitives, it's worth doing
                    // that calculation only once ahead of time before looping over tiles.
                    trace!("PreparedPrimitive {aabb:?} {prim_index:?}");
                    prepared_primitives.push(PreparedPrimitive { aabb, prim_index });
                    prim_index.0 += row_count;

                    continue;
                }

                // Batches are different; output the previous one before starting a new one.

                // Skip if batch is empty, which may happen on first one (current_batch
                // initialized to an invalid empty batch)
                if !current_batch.is_empty() {
                    // Assign primitives to tiles
                    extracted_canvas.tiles.assign_to_tiles(
                        &prepared_primitives[pp_offset as usize..],
                        extracted_canvas.screen_size.as_vec2(),
                    );
                    // trace!(
                    //     "{} primitives overlap {} tiles",
                    //     prepared_primitives.len() as u32 - pp_offset,
                    //     tile_count
                    // );

                    let oc_count = extracted_canvas.tiles.offset_and_count.len() as u32 - oc_offset;
                    current_batch.primitive_bind_group = BatchBuffers::Raw(oc_offset, oc_count);

                    trace!("Spawned new batch: oc_offset={oc_offset} oc_count={oc_count} pp_offset={pp_offset}");

                    commands.spawn(current_batch);

                    oc_offset += oc_count;
                    pp_offset = prepared_primitives.len() as u32;

                    // Align oc_offset to min_storage_buffer_offset_alignment
                    oc_offset = oc_offset.next_multiple_of(oc_align);
                    extracted_canvas
                        .tiles
                        .offset_and_count
                        .resize(oc_offset as usize, OffsetAndCount::default());
                }

                current_batch = new_batch;

                // Calculate once and save the AABB of the primitive, for tile assignment
                // purpose. Since there are many more tiles than primitives, it's worth doing
                // that calculation only once ahead of time before looping over tiles.
                trace!("PreparedPrimitive {aabb:?} {prim_index:?}");
                prepared_primitives.push(PreparedPrimitive { aabb, prim_index });
                prim_index.0 += row_count;
            }
        }

        // Output the last batch
        if !current_batch.is_empty() {
            trace!("Output last batch... pp_offset={pp_offset}");

            // Assign primitives to tiles
            extracted_canvas.tiles.assign_to_tiles(
                &prepared_primitives[pp_offset as usize..],
                extracted_canvas.screen_size.as_vec2(),
            );
            // trace!(
            //     "{} primitives overlap {} tiles",
            //     prepared_primitives.len() as u32 - pp_offset,
            //     tile_count
            // );

            let oc_count = extracted_canvas.tiles.offset_and_count.len() as u32 - oc_offset;
            current_batch.primitive_bind_group = BatchBuffers::Raw(oc_offset, oc_count);

            trace!("Spawned new batch: oc_offset={oc_offset} oc_count={oc_count} pp_offset={pp_offset}");

            commands.spawn(current_batch);
        }

        // Check the actual primitives after being assigned to tiles. There might be
        // primitives, but not visible on screen.
        if extracted_canvas.tiles.primitives.is_empty() {
            trace!("No primitive to render, finished preparing.");
            return;
        }

        // Write to GPU buffers
        trace!(
            "Writing {} elems for Canvas of entity {:?}",
            primitives.len(),
            entity
        );
        extracted_canvas.write_buffers(&primitives[..], &render_device, &render_queue);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn queue_primitives(
    views: Query<(Entity, &VisibleEntities, &ExtractedView)>,
    draw_functions: Res<DrawFunctions<Transparent2d>>,
    primitive_pipeline: Res<PrimitivePipeline>,
    mut pipelines: ResMut<SpecializedRenderPipelines<PrimitivePipeline>>,
    mut pipeline_cache: ResMut<PipelineCache>,
    msaa: Res<Msaa>,
    extracted_canvases: Res<ExtractedCanvases>,
    mut transparent_2d_render_phases: ResMut<ViewSortedRenderPhases<Transparent2d>>,
    batches: Query<(Entity, &PrimitiveBatch)>,
) {
    trace!("queue_primitives: {} batches", batches.iter().len());

    // TODO - per view culling?! (via VisibleEntities)
    trace!("Specializing pipeline(s)...");
    let draw_primitives_function = draw_functions.read().get_id::<DrawPrimitive>().unwrap();
    let key = PrimitivePipelineKey::from_msaa_samples(msaa.samples());
    let primitive_pipeline = pipelines.specialize(&mut pipeline_cache, &primitive_pipeline, key);
    trace!("primitive_pipeline={:?}", primitive_pipeline,);

    trace!("Looping on batches...");
    for (batch_entity, batch) in batches.iter() {
        trace!(
            "batch ent={:?} image={:?}",
            batch_entity,
            batch.image_handle_id
        );
        if batch.is_empty() {
            // shouldn't happen
            continue;
        }

        let canvas_entity = batch.canvas_entity;

        let is_textured = batch.image_handle_id != AssetId::<Image>::invalid();
        trace!("  is_textured={}", is_textured);

        let extracted_canvas =
            if let Some(extracted_canvas) = extracted_canvases.canvases.get(&canvas_entity) {
                extracted_canvas
            } else {
                continue;
            };

        trace!(
            "CanvasMeta: canvas_entity={:?} batch_entity={:?} textured={}",
            canvas_entity,
            batch_entity,
            is_textured,
        );

        let sort_key = FloatOrd(extracted_canvas.transform.translation().z);

        trace!("Looping on views...");
        for (view_entity, _visible_entities, _view) in views.iter() {
            let Some(render_phase) = transparent_2d_render_phases.get_mut(&view_entity) else {
                continue;
            };

            trace!(
                "Add Transparent2d entity={:?} image={:?} pipeline={:?} (sort={:?})",
                batch_entity,
                batch.image_handle_id,
                primitive_pipeline,
                sort_key
            );
            render_phase.add(Transparent2d {
                draw_function: draw_primitives_function,
                pipeline: primitive_pipeline,
                entity: batch_entity,
                sort_key,
                // This is batching multiple items into a single draw call, which is not a feature
                // of bevy_render we currently use
                batch_range: 0..1,
                extra_index: PhaseItemExtraIndex::NONE,
            });
        }
    }
}

pub fn prepare_bind_groups(
    render_device: Res<RenderDevice>,
    view_uniforms: Res<ViewUniforms>,
    primitive_pipeline: Res<PrimitivePipeline>,
    mut batches: Query<(Entity, &mut PrimitiveBatch)>,
    extracted_canvases: Res<ExtractedCanvases>,
    gpu_images: Res<RenderAssets<GpuImage>>,
    fallback_images: Res<FallbackImage>,
    mut primitive_meta: ResMut<PrimitiveMeta>,
    mut image_bind_groups: ResMut<ImageBindGroups>,
) {
    trace!("prepare_bind_groups()");

    let Some(view_binding) = view_uniforms.uniforms.binding() else {
        trace!("View binding not available; aborted.");
        return;
    };

    if image_bind_groups.fallback.is_none() {
        image_bind_groups.fallback = Some(render_device.create_bind_group(
            "keith:fallback_primitive_material_bind_group",
            &primitive_pipeline.material_layout,
            &[
                BindGroupEntry {
                    binding: 0,
                    resource: BindingResource::TextureView(&fallback_images.d2.texture_view),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: BindingResource::Sampler(&fallback_images.d2.sampler),
                },
            ],
        ));
        debug!(
            "Created bind group for fallback primitive texture: {:?}",
            image_bind_groups.fallback.as_ref().unwrap()
        );
    }

    primitive_meta.view_bind_group = Some(render_device.create_bind_group(
        "keith:primitive_view_bind_group",
        &primitive_pipeline.view_layout,
        &[BindGroupEntry {
            binding: 0,
            resource: view_binding,
        }],
    ));

    trace!("Looping on {} batches...", batches.iter().len());
    for (batch_entity, mut batch) in batches.iter_mut() {
        trace!(
            "batch ent={:?} image={:?}",
            batch_entity,
            batch.image_handle_id
        );
        if batch.is_empty() {
            // shouldn't happen
            continue;
        }

        let canvas_entity = batch.canvas_entity;

        let extracted_canvas =
            if let Some(extracted_canvas) = extracted_canvases.canvases.get(&canvas_entity) {
                extracted_canvas
            } else {
                warn!(
                    "Unknown extracted canvas entity {:?}. Skipped.",
                    canvas_entity
                );
                continue;
            };

        // There's no primitive overlapping any tile; skip any prepare.
        // FIXME - This should be more driven by batches; we shouldn't spawn empty
        // batches...
        if extracted_canvas.tiles.primitives.is_empty() {
            continue;
        }

        // The bind group should be reset each frame to BatchBuffers::Raw(), so anything
        // else is wrong
        let BatchBuffers::Raw(oc_offset, oc_size) = batch.primitive_bind_group else {
            warn!(
                "Batch buffers not ready: {:?}. Skipped.",
                batch.primitive_bind_group
            );
            continue;
        };

        let (Some(prim), Some(tile_prim), Some(oc)) = (
            extracted_canvas.binding(),
            extracted_canvas.tile_primitives_binding(),
            extracted_canvas.offset_and_count_binding(oc_offset, oc_size),
        ) else {
            warn!("Binding resource not ready. Skipped.");
            continue;
        };

        let primitive_bind_group = render_device.create_bind_group(
            Some(&format!("keith:prim_bind_group_{:?}", canvas_entity)[..]),
            &primitive_pipeline.prim_layout,
            &[
                BindGroupEntry {
                    binding: 0,
                    resource: prim,
                },
                BindGroupEntry {
                    binding: 1,
                    resource: tile_prim,
                },
                BindGroupEntry {
                    binding: 2,
                    resource: oc,
                },
            ],
        );
        debug!("Created bind group {primitive_bind_group:?} for batch on entity {batch_entity:?} with oc_offset={oc_offset} oc_size={oc_size}...");
        batch.primitive_bind_group = BatchBuffers::Prepared(primitive_bind_group);

        // Set bind group for texture, if any
        if batch.image_handle_id != AssetId::<Image>::invalid() {
            if let Some(gpu_image) = gpu_images.get(batch.image_handle_id) {
                image_bind_groups
                    .values
                    .entry(batch.image_handle_id)
                    .or_insert_with(|| {
                        debug!(
                            "Insert new bind group for handle={:?}",
                            batch.image_handle_id
                        );
                        render_device.create_bind_group(
                            "keith:primitive_material_bind_group",
                            &primitive_pipeline.material_layout,
                            &[
                                BindGroupEntry {
                                    binding: 0,
                                    resource: BindingResource::TextureView(&gpu_image.texture_view),
                                },
                                BindGroupEntry {
                                    binding: 1,
                                    resource: BindingResource::Sampler(&gpu_image.sampler),
                                },
                            ],
                        )
                    });
            } else {
                warn!(
                    "GPU image for asset {:?} is not available, cannot create bind group!",
                    batch.image_handle_id
                );
            }
        }
    }
}
