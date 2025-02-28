//! 🐕 Bevy Keith is a 2D graphics library exposing an immediate-mode style
//! API.
//!
//! # Quick start
//!
//! The central component is the [`Canvas`], which attached to a 2D [`Camera`]
//! stores the drawing commands enqueued each frame, then renders them.
//!
//! To draw on a [`Canvas`], you typically use the [`RenderContext`] helper.
//! From there you typically:
//! - create a [`Shape`], like [`Rect`] or [`RoundedRect`];
//! - create a [`Brush`] via _e.g._ [`RenderContext::solid_brush()`];
//! - fill the shape with the brush via [`RenderContext::fill()`].
//!
//! ```
//! # use bevy_keith::*;
//! # use bevy::{prelude::*, color::palettes::css::*};
//! fn draw(mut query: Query<&mut Canvas>) {
//!     let mut canvas = query.single_mut();
//!     canvas.clear();
//!     let mut ctx = canvas.render_context();
//!     let brush = ctx.solid_brush(RED.into());
//!     ctx.fill(Rect::from_center_size(Vec2::ZERO, Vec2::ONE), &brush);
//! }
//! ```
//!
//! # ⚠️ Disclaimer
//!
//! 🐕 Bevy Keith is still under development. Some known limitations include:
//!
//! - \[Feat\] Currently [`Canvas`] only reasonably works with a 2D orthographic
//!   camera. Other type of projections may work but are untested.
//! - \[Feat\] Only solid-color brushes are currently supported; no patterns or
//!   gradients.
//! - \[Feat\] The [`Canvas`] is rendered to Bevy's 2D main transparent pass;
//!   this means in particular that the Bevy UI, which is rendered later, will
//!   be rendered on top, so you cannot easily mix Bevy UI and this crate.
//! - \[Feat\] Text rendering uses pre-rasterized textured glyphs. SDF-based
//!   text would fit better and would offer extra features like text outlining,
//!   which are currently hard to implement with pre-rasterizing.
//! - \[Feat\] All [`Canvas`] are currently full-screen, with an origin centered
//!   on the screen. [`Canvas::rect`] is ignored; instead
//!   [`OrthographicProjection::area`] is used.
//! - \[Perf\] Images are not batched with text, so generate extra draw calls.
//!
//! # Features
//!
//! 🐕 Bevy Keith contains a renderer based on Signed Distance Fields (SDFs),
//! which are mathematical descriptions of shapes to draw. This is unlike more
//! standard renderers, like the built-in Bevy PBR renderer, which work with
//! triangle-based meshes. An SDF representation is similar to vector graphics,
//! and offers the advantage that the shape can be arbitrarily zoomed in and out
//! without any loss of precision or aliasing. SDFs also enable various features
//! like outlining and glow on any kind of shape (TODO; not yet implemented).
//!
//! Currently, text rendering uses pre-rasterized glyphs stored in a texture
//! atlas, and therefore can suffer from aliasing if zoomed in too much.
//!
//! [`Brush`]: crate::render_context::Brush

use bevy::{
    asset::load_internal_asset,
    core_pipeline::core_2d::Transparent2d,
    prelude::*,
    render::{
        render_phase::AddRenderCommand,
        render_resource::{Shader, SpecializedRenderPipelines},
        texture::GpuImage,
        Render, RenderApp, RenderSet,
    },
};

pub mod canvas;
mod render;
pub mod render_context;
pub mod shapes;
pub mod text;

pub mod prelude {
    #[doc(hidden)]
    pub use crate::*;
}

pub use canvas::{Canvas, Primitive, TileConfig};
use render::{
    DrawPrimitive, ExtractedCanvases, ImageBindGroups, PrimitiveAssetEvents, PrimitiveMeta,
    PrimitivePipeline,
};
pub use render_context::{ImageScaling, RenderContext};
pub use shapes::*;
pub use text::{CanvasTextId, KeithTextPipeline};

/// Main Keith plugin.
#[derive(Default)]
pub struct KeithPlugin;

/// Reference to the primitive shader `prim.wgsl`, embedded in the code.
pub(crate) const PRIMITIVE_SHADER_HANDLE: Handle<Shader> =
    Handle::weak_from_u128(1713353953151292643);

/// System sets for Keith.
#[derive(Debug, Hash, PartialEq, Eq, Clone, SystemSet)]
pub enum KeithSystem {
    /// Label for [`text::process_glyphs()`].
    ProcessTextGlyphs,

    /// Spawn any [`Tiles`] or [`TileConfig`] component where missing.
    ///
    /// This executes as part of the [`PostUpdate`] schedule.
    ///
    /// [`Tiles`]: crate::canvas::Tiles
    SpawnMissingTilesComponents,

    /// Resize the [`Tiles`] component of a [`Canvas`] to accomodate the size of
    /// the render target of a [`Camera`].
    ///
    /// [`Tiles`]: crate::canvas::Tiles
    // FIXME - Currently a canvas always targets the full camera screen size.
    ResizeTilesToCameraRenderTarget,

    /// Extract the render commands stored this frame in all the [`Canvas`], to
    /// prepare for rendering.
    ExtractPrimitives,
}

impl Plugin for KeithPlugin {
    fn build(&self, app: &mut App) {
        load_internal_asset!(
            app,
            PRIMITIVE_SHADER_HANDLE,
            "render/prim.wgsl",
            Shader::from_wgsl
        );

        app.init_resource::<KeithTextPipeline>()
            .add_systems(PreUpdate, canvas::update_canvas_from_ortho_camera)
            .add_systems(PostUpdate, text::process_glyphs)
            .configure_sets(
                PostUpdate,
                (
                    KeithSystem::SpawnMissingTilesComponents,
                    KeithSystem::ResizeTilesToCameraRenderTarget,
                )
                    .chain()
                    // We need the result of the positioned glyphs to be able to assign them to
                    // tiles
                    .after(text::process_glyphs),
            )
            .add_systems(
                PostUpdate,
                (
                    canvas::spawn_missing_tiles_components
                        .in_set(KeithSystem::SpawnMissingTilesComponents),
                    canvas::resize_tiles_to_camera_render_target
                        .in_set(KeithSystem::ResizeTilesToCameraRenderTarget)
                        .after(bevy::transform::TransformSystem::TransformPropagate)
                        .after(bevy::render::view::VisibilitySystems::CheckVisibility)
                        .after(bevy::render::camera::CameraUpdateSystem),
                    canvas::allocate_atlas_layouts,
                    canvas::process_images,
                ),
            );
    }

    fn finish(&self, app: &mut App) {
        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            render_app
                .init_resource::<ImageBindGroups>()
                .init_resource::<PrimitivePipeline>()
                .init_resource::<SpecializedRenderPipelines<PrimitivePipeline>>()
                .init_resource::<PrimitiveMeta>()
                .init_resource::<ExtractedCanvases>()
                .init_resource::<PrimitiveAssetEvents>()
                .add_render_command::<Transparent2d, DrawPrimitive>()
                .configure_sets(ExtractSchedule, KeithSystem::ExtractPrimitives)
                .edit_schedule(ExtractSchedule, |schedule| {
                    schedule.add_systems(
                        (
                            render::extract_primitives,
                            render::extract_primitive_events,
                            // text::extract_text_primitives
                        )
                            .in_set(KeithSystem::ExtractPrimitives)
                            .after(bevy::sprite::SpriteSystem::ExtractSprites),
                    );
                })
                .add_systems(
                    Render,
                    (
                        render::prepare_primitives
                            .in_set(RenderSet::PrepareAssets)
                            .after(KeithSystem::ExtractPrimitives)
                            .after(bevy::text::extract_text2d_sprite),
                        render::queue_primitives
                            .in_set(RenderSet::Queue)
                            .after(render::prepare_primitives)
                            .before(bevy::render::render_phase::sort_phase_system::<Transparent2d>),
                        render::prepare_bind_groups
                            .in_set(RenderSet::PrepareBindGroups)
                            .after(render::queue_primitives)
                            .after(bevy::render::render_asset::prepare_assets::<GpuImage>),
                    ),
                );
        };
    }
}
