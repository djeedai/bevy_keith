//! A canvas represents the drawing surface storing draw commands.
//!
//! To prepare to draw with 🐕 Bevy Keith, add a [`Canvas`] component to the
//! same [`Entity`] as a [`Camera`]. Currently only 2D orthographic cameras are
//! supported.
//!
//! In general, you don't need to interact directly with a [`Canvas`] to draw.
//! Instead, the [`RenderContext`] exposes a more convenient interface on top of
//! a specific [`Canvas`]. Simply retrieve the render context for an existing
//! canvas and use it to enqueue draw commands.
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
//! At the end of each frame, the render commands stored in the [`Canvas`] are
//! extracted into the render app and drawn. Then the command list is flushed.
//! Commands are not reused from one frame to the other; you need to redraw each
//! frame ("immediate-mode" style rendering).

use std::mem::MaybeUninit;

use bevy::{
    asset::{AssetId, Assets, Handle},
    color::Color,
    ecs::{
        component::Component,
        entity::Entity,
        query::{With, Without},
        system::{Commands, Query, ResMut},
    },
    log::trace,
    math::{bounding::Aabb2d, Rect, UVec2, Vec2, Vec3},
    prelude::*,
    render::{camera::Camera, texture::Image},
    sprite::TextureAtlasLayout,
    utils::default,
    window::PrimaryWindow,
};
use bytemuck::{Pod, Zeroable};

use crate::{
    render::{ExtractedCanvas, ExtractedText, PreparedPrimitive},
    render_context::{ImageScaling, RenderContext, TextLayout},
    ShapeRef,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PrimitiveInfo {
    /// Row count per sub-primitive.
    pub row_count: u32,
    /// Number of sub-primitives.
    pub sub_prim_count: u32,
}

/// Kind of primitives understood by the GPU shader.
///
/// Determines the shader path and the SDF function to use to render a
/// primitive. Each primitive has a different shader encoding and
/// functionalities.
///
/// # Note
///
/// The enum values must be kept in sync with the values inside the primitive
/// shader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GpuPrimitiveKind {
    /// Axis-aligned rectangle, possibly textured.
    Rect = 0,
    /// Text glyph. Same as `Rect`, but samples from texture's alpha instead of
    /// RGB, and is always textured.
    Glyph = 1,
    /// Line segment.
    Line = 2,
    /// Quarter pie.
    QuarterPie = 3,
}

/// Drawing primitives.
///
/// The drawing primitives are the lowest-level concepts mapping directly to
/// shader instructions. For the higher level shapes to draw on a [`Canvas`],
/// see the [`shapes`] module instead.
///
/// [`shapes`]: crate::shapes
#[derive(Debug, Clone, Copy)]
pub enum Primitive {
    /// A line between two points, with a color and thickness.
    Line(LinePrimitive),
    /// An axis-aligned rectangle with a color, optional rounded corners, and
    /// optional texture.
    Rect(RectPrimitive),
    /// A text with a color.
    Text(TextPrimitive),
    QuarterPie(QuarterPiePrimitive),
}

impl Primitive {
    /// Get the [`GpuPrimitiveKind`] of a primitive.
    pub fn gpu_kind(&self) -> GpuPrimitiveKind {
        match self {
            Primitive::Line(_) => GpuPrimitiveKind::Line,
            Primitive::Rect(_) => GpuPrimitiveKind::Rect,
            Primitive::Text(_) => GpuPrimitiveKind::Glyph,
            Primitive::QuarterPie(_) => GpuPrimitiveKind::QuarterPie,
        }
    }

    /// Get the AABB of a primitive.
    ///
    /// This is mainly used internally for tiling. There's no guarantee that the
    /// AABB is tightly fitting; instead it only needs to be conservative and
    /// enclose all the primitive.
    pub fn aabb(&self) -> Aabb2d {
        match self {
            Primitive::Line(l) => l.aabb(),
            Primitive::Rect(r) => r.aabb(),
            Primitive::Text(_) => panic!("Cannot compute text AABB intrinsically."),
            Primitive::QuarterPie(q) => q.aabb(),
        }
    }

    /// Is the primitive textured?
    pub fn is_textured(&self) -> bool {
        match self {
            Primitive::Line(_) => false,
            Primitive::Rect(r) => r.is_textured(),
            Primitive::Text(_) => false, // not in the sense of regular texture mapping
            Primitive::QuarterPie(_) => false,
        }
    }

    /// Is the primitive bordered?
    pub fn is_bordered(&self) -> bool {
        match self {
            Primitive::Line(l) => l.is_bordered(),
            Primitive::Rect(r) => r.is_bordered(),
            Primitive::Text(_) => false,
            Primitive::QuarterPie(_) => false,
        }
    }

    /// Internal primitive info for drawing a primitive.
    pub(crate) fn info(&self, texts: &[ExtractedText]) -> PrimitiveInfo {
        match &self {
            Primitive::Line(l) => l.info(),
            Primitive::Rect(r) => r.info(),
            Primitive::Text(t) => t.info(texts),
            Primitive::QuarterPie(q) => q.info(),
        }
    }

    /// Serialize a primitive and write its binary blob into the given buffer,
    /// ready to be consumed by the GPU shader.
    ///
    /// Anything written here must be kept in sync format-wise with what is read
    /// back in the shader.
    pub(crate) fn write(
        &self,
        texts: &[ExtractedText],
        prim: &mut [MaybeUninit<f32>],
        canvas_translation: Vec2,
        scale_factor: f32,
    ) {
        match &self {
            Primitive::Line(l) => l.write(prim, canvas_translation, scale_factor),
            Primitive::Rect(r) => r.write(prim, canvas_translation, scale_factor),
            Primitive::Text(t) => t.write(texts, prim, canvas_translation, scale_factor),
            Primitive::QuarterPie(q) => q.write(prim, canvas_translation, scale_factor),
        };
    }
}

impl From<LinePrimitive> for Primitive {
    fn from(line: LinePrimitive) -> Self {
        Self::Line(line)
    }
}

impl From<RectPrimitive> for Primitive {
    fn from(rect: RectPrimitive) -> Self {
        Self::Rect(rect)
    }
}

impl From<TextPrimitive> for Primitive {
    fn from(text: TextPrimitive) -> Self {
        Self::Text(text)
    }
}

impl From<QuarterPiePrimitive> for Primitive {
    fn from(qpie: QuarterPiePrimitive) -> Self {
        Self::QuarterPie(qpie)
    }
}

/// A line between two points, with a color and thickness.
///
/// This is essentially an oriented rectangle.
#[derive(Debug, Default, Clone, Copy)]
pub struct LinePrimitive {
    /// The starting point of the line.
    pub start: Vec2,
    /// The ending point of the line.
    pub end: Vec2,
    /// The line color.
    pub color: Color,
    /// The line thickness. Must be greater than zero.
    ///
    /// The line shape extends equally by `thickness / 2.` on both sides of the
    /// mathematical (infinitely thin) line joining the start and end points.
    pub thickness: f32,
    /// Size of the border, if any, or zero if no border. The borders always
    /// expand inside the line. Negative values or zero mean no border.
    pub border_width: f32,
    /// Border color, if any (ignored if `border_width <= 0.`).
    pub border_color: Color,
}

impl LinePrimitive {
    /// The AABB of the line primitive.
    pub fn aabb(&self) -> Aabb2d {
        let dir = (self.end - self.start).normalize();
        let tg = Vec2::new(-dir.y, dir.x);
        let e = self.thickness / 2.;
        let p0 = self.start + tg * e;
        let p1 = self.start - tg * e;
        let p2 = self.end + tg * e;
        let p3 = self.end - tg * e;
        let min = p0.min(p1).min(p2).min(p3);
        let max = p0.max(p1).max(p2).max(p3);
        Aabb2d { min, max }
    }

    /// Is the primitive bordered?
    pub fn is_bordered(&self) -> bool {
        self.border_width > 0.
    }

    fn info(&self) -> PrimitiveInfo {
        PrimitiveInfo {
            row_count: 6 + if self.is_bordered() { 2 } else { 0 },
            sub_prim_count: 1,
        }
    }

    fn write(&self, prim: &mut [MaybeUninit<f32>], canvas_translation: Vec2, scale_factor: f32) {
        prim[0].write((self.start.x + canvas_translation.x) * scale_factor);
        prim[1].write((self.start.y + canvas_translation.y) * scale_factor);
        prim[2].write((self.end.x + canvas_translation.x) * scale_factor);
        prim[3].write((self.end.y + canvas_translation.y) * scale_factor);
        prim[4].write(bytemuck::cast(self.color.to_linear().as_u32()));
        prim[5].write(self.thickness * scale_factor);
        if self.is_bordered() {
            assert_eq!(8, prim.len());
            prim[6].write(self.border_width * scale_factor);
            prim[7].write(bytemuck::cast(self.border_color.to_linear().as_u32()));
        } else {
            assert_eq!(6, prim.len());
        }
    }
}

/// An axis-aligned rectangle with a color, optional rounded corners, and
/// optional texture.
#[derive(Debug, Default, Clone, Copy)]
pub struct RectPrimitive {
    /// Position and size of the rectangle in its canvas space.
    ///
    /// For rounded rectangles, this is the AABB (the radius and borders are
    /// included).
    pub rect: Rect,
    /// Rounded corners radius. Set to zero to disable rounded corners.
    pub radius: f32,
    /// Uniform rectangle color.
    pub color: Color,
    /// Optional handle to the image used for texturing the rectangle.
    pub image: Option<AssetId<Image>>,
    /// Image size, populated from actual texture size and scaling.
    pub image_size: Vec2,
    /// Scaling for the image (if any).
    pub image_scaling: ImageScaling,
    /// Flip the image (if any) along the horizontal axis.
    pub flip_x: bool,
    /// Flip the image (if any) along the vertical axis.
    pub flip_y: bool,
    /// Size of the border, if any, or zero if no border. The borders always
    /// expand inside the rectangle. Negative values or zero mean no border.
    pub border_width: f32,
    /// Border color, if any (ignored if `border_width <= 0.`).
    pub border_color: Color,
}

impl RectPrimitive {
    /// Number of primitive buffer rows (4 bytes) per primitive.
    const ROW_COUNT_BASE: u32 = 6;
    /// Number of extra primitive buffer rows (4 bytes) per primitive to add
    /// when textured. Those extra rows follow the base ones.
    const ROW_COUNT_TEX: u32 = 4;
    /// Number of extra primitive buffer rows (4 bytes) per primitive to add
    /// when bordered. Those extra rows follow the texture ones, or the base
    /// ones if there's no texture.
    const ROW_COUNT_BORDER: u32 = 2;

    /// Get the AABB of this rectangle.
    pub fn aabb(&self) -> Aabb2d {
        Aabb2d {
            min: self.rect.min,
            max: self.rect.max,
        }
    }

    /// Is this primitive textured?
    ///
    /// True if [`RectPrimitive::image`] is `Some`.
    pub const fn is_textured(&self) -> bool {
        self.image.is_some()
    }

    /// Is the primitive bordered?
    pub fn is_bordered(&self) -> bool {
        self.border_width > 0.
    }

    #[inline]
    fn row_count(&self) -> u32 {
        let mut rows = Self::ROW_COUNT_BASE;
        if self.is_textured() {
            rows += Self::ROW_COUNT_TEX;
        }
        if self.is_bordered() {
            rows += Self::ROW_COUNT_BORDER;
        }
        rows
    }

    fn info(&self) -> PrimitiveInfo {
        PrimitiveInfo {
            row_count: self.row_count(),
            sub_prim_count: 1,
        }
    }

    fn write(&self, prim: &mut [MaybeUninit<f32>], canvas_translation: Vec2, scale_factor: f32) {
        assert_eq!(
            self.row_count() as usize,
            prim.len(),
            "Invalid buffer size {} to write RectPrimitive (needs {})",
            prim.len(),
            self.row_count()
        );

        let half_min = self.rect.min * (0.5 * scale_factor);
        let half_max = self.rect.max * (0.5 * scale_factor);
        let center = half_min + half_max + canvas_translation * scale_factor;
        let half_size = half_max - half_min;
        prim[0].write(center.x);
        prim[1].write(center.y);
        prim[2].write(half_size.x);
        prim[3].write(half_size.y);
        prim[4].write(self.radius * scale_factor);
        prim[5].write(bytemuck::cast(self.color.to_linear().as_u32()));
        let mut idx = 6;
        if self.is_textured() {
            prim[idx + 0].write(0.5);
            prim[idx + 1].write(0.5);
            prim[idx + 2].write(1. / self.image_size.x);
            prim[idx + 3].write(1. / self.image_size.y);
            idx += 4;
        }
        if self.is_bordered() {
            prim[idx + 0].write(self.border_width * scale_factor);
            prim[idx + 1].write(bytemuck::cast(self.border_color.to_linear().as_u32()));
        }
    }
}

/// A reference to a text with a color.
///
/// The text primitive is not stored directly inside this struct. Instead, the
/// struct stores an [`id`] field indexing the text into its [`Canvas`]. This
/// extra indirection allows storing all texts together for convenience, as they
/// require extra pre-processing compared to other primitives.
///
/// [`id`]: crate::canvas::TextPrimitive::id
#[derive(Debug, Clone, Copy)]
pub struct TextPrimitive {
    /// Unique ID of the text inside its owner [`Canvas`].
    pub id: u32,
    /// TODO - Vec2 instead?
    pub rect: Rect,
}

impl TextPrimitive {
    /// Number of elements used by each single glyph in the primitive element
    /// buffer.
    pub const ROW_PER_GLYPH: u32 = RectPrimitive::ROW_COUNT_BASE + RectPrimitive::ROW_COUNT_TEX;

    /// Get the AABB of this text.
    pub fn aabb(&self, canvas: &ExtractedCanvas) -> Aabb2d {
        let text = &canvas.texts[self.id as usize];
        let mut aabb = Aabb2d {
            min: self.rect.min,
            max: self.rect.max,
        };
        trace!("Text #{:?} aabb={:?}", self.id, aabb);
        for glyph in &text.glyphs {
            aabb.min = aabb.min.min(self.rect.min + glyph.offset);
            aabb.max = aabb.max.max(self.rect.min + glyph.offset + glyph.size);
            trace!(
                "  > add glyph offset={:?} size={:?}, new aabb {:?}",
                glyph.offset,
                glyph.size,
                aabb
            );
        }
        aabb
    }

    fn info(&self, texts: &[ExtractedText]) -> PrimitiveInfo {
        let index = self.id as usize;
        if index < texts.len() {
            let glyph_count = texts[index].glyphs.len() as u32;
            PrimitiveInfo {
                row_count: Self::ROW_PER_GLYPH,
                sub_prim_count: glyph_count,
            }
        } else {
            PrimitiveInfo {
                row_count: 0,
                sub_prim_count: 0,
            }
        }
    }

    fn write(
        &self,
        texts: &[ExtractedText],
        prim: &mut [MaybeUninit<f32>],
        canvas_translation: Vec2,
        scale_factor: f32,
    ) {
        let index = self.id as usize;
        let glyphs = &texts[index].glyphs;
        let glyph_count = glyphs.len();
        assert_eq!(glyph_count * Self::ROW_PER_GLYPH as usize, prim.len());
        let mut ip = 0;
        //let inv_scale_factor = 1. / scale_factor;
        for i in 0..glyph_count {
            let x = glyphs[i].offset.x + (self.rect.min.x + canvas_translation.x) * scale_factor;
            let y = glyphs[i].offset.y + (self.rect.min.y + canvas_translation.y) * scale_factor;
            let hw = glyphs[i].size.x / 2.0;
            let hh = glyphs[i].size.y / 2.0;

            // let x = x * inv_scale_factor;
            // let y = y * inv_scale_factor;
            // let hw = hw * inv_scale_factor;
            // let hh = hh * inv_scale_factor;

            // Glyph position is center of rect, we need bottom-left corner
            //let x = x - w / 2.;
            //let y = y - h / 2.;

            // FIXME - hard-coded texture size
            let uv_x = glyphs[i].uv_rect.min.x / 1024.0;
            let uv_y = glyphs[i].uv_rect.min.y / 1024.0;
            let uv_w = glyphs[i].uv_rect.max.x / 1024.0 - uv_x;
            let uv_h = glyphs[i].uv_rect.max.y / 1024.0 - uv_y;

            // Glyph UV is flipped vertically
            // let uv_y = uv_y + uv_h;
            // let uv_h = -uv_h;

            // center pos
            // we round() here to work around a bug: if the pixel rect is not aligned on the
            // screen pixel grid, the UV coordinates may end up being < 0.5 or >
            // w + 0.5, which then bleeds into adjacent pixels. it looks like
            // the rasterizing of the glyphs already adds 1 pixel border, so we should
            // remove that border in the SDF rect, so that we never sample the
            // texture beyond half that 1 px border, which would linearly blend
            // with the next pixel (outside the glyph rect).
            prim[ip + 0].write(x.round() + hw);
            prim[ip + 1].write(y.round() + hh);

            // half size
            prim[ip + 2].write(hw);
            prim[ip + 3].write(hh);

            // radius
            prim[ip + 4].write(0.);

            // color
            prim[ip + 5].write(bytemuck::cast(glyphs[i].color));

            // uv_offset (at center pos)
            prim[ip + 6].write(uv_x + uv_w / 2.0);
            prim[ip + 7].write(uv_y + uv_h / 2.0);

            // uv_scale
            prim[ip + 8].write(1.0 / 1024.0);
            prim[ip + 9].write(1.0 / 1024.0);

            ip += Self::ROW_PER_GLYPH as usize;
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct QuarterPiePrimitive {
    /// Origin of the pie.
    pub origin: Vec2,
    /// Radii of the (elliptical) pie.
    pub radii: Vec2,
    /// Uniform rectangle color.
    pub color: Color,
    /// Flip the quarter pie along the horizontal axis.
    pub flip_x: bool,
    /// Flip the quarter pie along the vertical axis.
    pub flip_y: bool,
}

impl Default for QuarterPiePrimitive {
    fn default() -> Self {
        Self {
            origin: Vec2::ZERO,
            radii: Vec2::ONE,
            color: Color::default(),
            flip_x: false,
            flip_y: false,
        }
    }
}

impl QuarterPiePrimitive {
    /// Number of primitive buffer rows (4 bytes) per primitive.
    const ROW_COUNT: u32 = 5;

    pub fn aabb(&self) -> Aabb2d {
        Aabb2d {
            min: self.origin - self.radii,
            max: self.origin + self.radii,
        }
    }

    /// The pie center.
    pub fn center(&self) -> Vec3 {
        self.origin.extend(0.)
    }

    #[inline]
    const fn row_count(&self) -> u32 {
        Self::ROW_COUNT
    }

    fn info(&self) -> PrimitiveInfo {
        PrimitiveInfo {
            row_count: self.row_count(),
            sub_prim_count: 1,
        }
    }

    fn write(&self, prim: &mut [MaybeUninit<f32>], canvas_translation: Vec2, scale_factor: f32) {
        assert_eq!(self.row_count() as usize, prim.len());
        let radii_mask = BVec2::new(self.flip_x, self.flip_y);
        let signed_radii = Vec2::select(radii_mask, -self.radii, self.radii);
        prim[0].write((self.origin.x + canvas_translation.x) * scale_factor);
        prim[1].write((self.origin.y + canvas_translation.y) * scale_factor);
        prim[2].write(signed_radii.x * scale_factor);
        prim[3].write(signed_radii.y * scale_factor);
        prim[4].write(bytemuck::cast(self.color.to_linear().as_u32()));
    }
}

/// Drawing surface for 2D graphics.
///
/// This component should attached to the same entity as a [`Camera`] and an
/// [`OrthographicProjection`].
///
/// By default the dimensions of the canvas are automatically computed and
/// updated based on that projection.
#[derive(Component)]
pub struct Canvas {
    /// The canvas dimensions relative to its origin.
    ///
    /// Currently ignored.
    rect: Rect,
    /// Optional background color to clear the canvas with.
    ///
    /// This only has an effect starting from the next [`clear()`] call. If a
    /// background color is set, it's used to clear the canvas each frame.
    /// Otherwise, the canvas retains its default transparent black color (0.0,
    /// 0.0, 0.0, 0.0).
    ///
    /// [`clear()`]: crate::Canvas::clear
    pub background_color: Option<Color>,
    /// Collection of drawn primitives.
    primitives: Vec<Primitive>,
    /// Collection of allocated texts.
    pub(crate) text_layouts: Vec<TextLayout>,
    /// Atlas layout. Needs to be a separate asset resource due to Bevy's API
    /// only.
    pub(crate) atlas_layout: Handle<TextureAtlasLayout>,
}

impl Default for Canvas {
    fn default() -> Self {
        Self {
            rect: Rect::default(),
            background_color: None,
            primitives: vec![],
            text_layouts: vec![],
            atlas_layout: Handle::default(),
        }
    }
}

impl Canvas {
    /// Create a new canvas with given dimensions.
    ///
    /// FIXME - Currently the rectangle is ignored; all canvases are
    /// full-screen.
    pub fn new(rect: Rect) -> Self {
        Self { rect, ..default() }
    }

    /// Change the dimensions of the canvas.
    ///
    /// This is called automatically if the [`Canvas`] is on the same entity as
    /// an [`OrthographicProjection`].
    pub fn set_rect(&mut self, rect: Rect) {
        // if let Some(color) = self.background_color {
        //     if self.rect != rect {
        //         TODO - clear new area if any? or resize the clear() rect?!
        //     }
        // }
        self.rect = rect;
    }

    /// Get the dimensions of the canvas relative to its origin.
    ///
    /// FIXME - Currently this is always [`OrthographicProjection::area`].
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// Clear the canvas, discarding all primitives previously drawn on it.
    ///
    /// If the canvas has a [`background_color`], this clears the canvas to that
    /// color.
    ///
    /// [`background_color`]: Canvas::background_color
    pub fn clear(&mut self) {
        self.primitives.clear();
        self.text_layouts.clear(); // FIXME - really?

        if let Some(color) = self.background_color {
            self.draw(RectPrimitive {
                rect: self.rect,
                color,
                ..default()
            });
        }
    }

    /// Draw a new primitive onto the canvas.
    ///
    /// This is a lower level entry point to canvas drawing; in general, you
    /// should prefer acquiring a [`RenderContext`] via [`render_context()`]
    /// and using it to draw primitives.
    ///
    /// [`render_context()`]: crate::canvas::Canvas::render_context
    #[inline]
    pub fn draw<'a>(&'a mut self, prim: impl Into<Primitive>) -> ShapeRef<'a> {
        let prim = prim.into();
        self.primitives.push(prim);
        let sref = ShapeRef {
            prim: self.primitives.last_mut().unwrap(),
        };
        sref
    }

    /// Acquire a new render context to draw on this canvas.
    pub fn render_context(&mut self) -> RenderContext {
        RenderContext::new(self)
    }

    pub(crate) fn finish(&mut self) {
        //
    }

    pub(crate) fn finish_layout(&mut self, mut layout: TextLayout) -> u32 {
        let id = self.text_layouts.len() as u32;
        trace!("finish_layout() for text #{}", id);
        layout.id = id;
        self.text_layouts.push(layout);
        id
    }

    // Workaround for Extract phase without mut access to MainWorld Canvas
    pub(crate) fn buffer(&self) -> &Vec<Primitive> {
        &self.primitives
    }

    pub(crate) fn text_layouts(&self) -> &[TextLayout] {
        &self.text_layouts[..]
    }

    pub(crate) fn text_layouts_mut(&mut self) -> &mut [TextLayout] {
        &mut self.text_layouts[..]
    }

    pub(crate) fn has_text(&self) -> bool {
        !self.text_layouts.is_empty()
    }
}

/// Update the dimensions of any [`Canvas`] component attached to the same
/// entity as as an [`OrthographicProjection`] component.
///
/// This runs in the [`PreUpdate`] schedule.
///
/// [`PreUpdate`]: bevy::app::PreUpdate
pub fn update_canvas_from_ortho_camera(mut query: Query<(&mut Canvas, &OrthographicProjection)>) {
    trace!("PreUpdate: update_canvas_from_ortho_camera()");
    for (mut canvas, ortho) in query.iter_mut() {
        trace!("ortho canvas rect = {:?}", ortho.area);
        canvas.set_rect(ortho.area);
    }
}

/// Configuration for tile-based rendering.
///
/// Currently unused.
#[derive(Default, Clone, Copy, Component)]
pub struct TileConfig {}

#[derive(Debug, Default, Clone, Copy, Pod, Zeroable)]
#[repr(C)]
pub(crate) struct OffsetAndCount {
    /// Base index into [`Tiles::primitives`].
    pub offset: u32,
    /// Number of consecutive primitive offsets in [`Tiles::primitives`].
    pub count: u32,
}

/// Packed primitive index and extra data.
///
/// Contains a primitive index packed inside a `u32` alongside other bits
/// necessary to drive the shader code:
/// - Index of the first row in the primitive buffer.
/// - Kind of primitive.
/// - Is the primitive textured?
/// - Is the primitive bordered (has a border)?
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
#[repr(transparent)]
pub(crate) struct PackedPrimitiveIndex(pub u32);

impl PackedPrimitiveIndex {
    /// Create a new packed index from individual values.
    pub fn new(index: u32, kind: GpuPrimitiveKind, textured: bool, bordered: bool) -> Self {
        let textured = (textured as u32) << 31;
        let bordered = (bordered as u32) << 27;
        let value = (index & 0x07FF_FFFF) | (kind as u32) << 28 | textured | bordered;
        Self(value)
    }
}

#[derive(Clone, Copy)]
struct AssignedTile {
    pub tile_index: i32,
    pub prim_index: PackedPrimitiveIndex,
}

/// Component storing per-tile draw data.
///
/// This component is automatically added to any [`Camera`] and [`Canvas`]
/// component pair missing it. It stores per-tile runtime data for rendering the
/// canvas primitives. Most users can ignore it entirely.
#[derive(Default, Clone, Component)]
pub struct Tiles {
    /// Tile size, in pixels. Currently hard-coded to 8x8 pixels.
    pub(crate) tile_size: UVec2,
    /// Dimensions of the canvas, in number of tiles.
    ///
    /// 4K, 8x8 => 129'600 tiles
    /// 1080p, 8x8 => 32'400 tiles
    pub(crate) dimensions: UVec2,
    /// Flattened list of primitive indices for each tile. The start of a tile
    /// is at element [`OffsetAndCount::offset`], and the tile contains
    /// [`OffsetAndCount::count`] consecutive primitive offsets, each offset
    /// being the start of the primitive into the primitive buffer of the
    /// canvas.
    pub(crate) primitives: Vec<PackedPrimitiveIndex>,
    /// Offset and count of primitives per tile, into [`Tiles::primitives`].
    pub(crate) offset_and_count: Vec<OffsetAndCount>,
    /// Local cache saved frame-to-frame to avoid allocations.
    assigned_tiles: Vec<AssignedTile>,
}

impl Tiles {
    /// Update the tile data based on the current screen (canvas) size.
    ///
    /// This recalculates the dimensions of the various buffers and reallocate
    /// them, to prepare for tiled drawing.
    pub fn update_size(&mut self, screen_size: UVec2) {
        // We force a 8x8 pixel tile, which works well with 32- and 64- waves.
        self.tile_size = UVec2::new(8, 8);

        self.dimensions = (screen_size.as_vec2() / self.tile_size.as_vec2())
            .ceil()
            .as_uvec2();

        assert!(self.dimensions.x * self.tile_size.x >= screen_size.x);
        assert!(self.dimensions.y * self.tile_size.y >= screen_size.y);

        self.primitives.clear();
        self.offset_and_count.clear();
        self.offset_and_count
            .reserve(self.dimensions.x as usize * self.dimensions.y as usize);

        trace!(
            "Resized Tiles at tile_size={:?} dim={:?} and cleared buffers",
            self.tile_size,
            self.dimensions
        );
    }

    /// Assign the given primitives to tiles.
    ///
    /// This performs the actual binning of primitives into one or more tiles.
    /// This assumes the various tile buffers are appropriately sized and
    /// allocated by a previous call to [`update_size()`].
    ///
    /// [`update_size()`]: crate::canvas::Tiles::update_size
    pub(crate) fn assign_to_tiles(&mut self, primitives: &[PreparedPrimitive], screen_size: Vec2) {
        let tile_size = self.tile_size.as_vec2();

        let oc_extra = self.dimensions.x as usize * self.dimensions.y as usize;
        self.offset_and_count.reserve(oc_extra);

        // Some semi-random guesswork of average tile overlapping count per primitive,
        // so we don't start from a stupidly small allocation.
        self.assigned_tiles.reserve(primitives.len() * 4);

        // Loop over primitives and find tiles they overlap
        for prim in primitives {
            // Calculate bounds in terms of tile indices, clamped to the size of the screen
            let uv_min = (prim.aabb.min.clamp(Vec2::ZERO, screen_size) / tile_size)
                .floor()
                .as_ivec2();
            let mut uv_max = (prim.aabb.max.clamp(Vec2::ZERO, screen_size) / tile_size)
                .ceil()
                .as_ivec2();
            if prim.aabb.max.x == tile_size.x * uv_max.x as f32 {
                // We ignore tiles which only have a shared edge and no actualy surface overlap
                uv_max.x -= 1;
            }
            if prim.aabb.max.y == tile_size.y * uv_max.y as f32 {
                // We ignore tiles which only have a shared edge and no actualy surface overlap
                uv_max.y -= 1;
            }

            self.assigned_tiles
                .reserve((uv_max.y - uv_min.y + 1) as usize * (uv_max.x - uv_min.x + 1) as usize);

            // Loop on tiles overlapping this primitive. This is generally only a handful,
            // unless the primitive covers a large part of the screen.
            for ty in uv_min.y..=uv_max.y {
                let base_tile_index = ty * self.dimensions.x as i32;
                for tx in uv_min.x..=uv_max.x {
                    let tile_index = base_tile_index + tx;
                    self.assigned_tiles.push(AssignedTile {
                        tile_index,
                        prim_index: prim.prim_index,
                    });
                }
            }
        }

        // Sort the primitive<->tile mapping by tile index. Note that the sort MUST BE
        // STABLE, to preserve the order of primitives, which preserves what is drawn on
        // top of what.
        self.assigned_tiles.sort_by_key(|at| at.tile_index);

        // Build the offset and count list
        self.primitives.reserve(self.assigned_tiles.len());
        let mut ti = -1;
        let mut offset = 0;
        let mut count = 0;
        for at in &self.assigned_tiles {
            if at.tile_index != ti {
                if count > 0 {
                    // Write previous tile
                    self.offset_and_count.push(OffsetAndCount {
                        offset: offset as u32,
                        count,
                    });
                }
                // Write empty tile(s)
                for _ in ti + 1..at.tile_index {
                    self.offset_and_count.push(OffsetAndCount {
                        offset: offset as u32,
                        count: 0,
                    });
                }
                offset = self.primitives.len() as u32;
                count = 0;
                ti = at.tile_index;
            }

            self.primitives.push(at.prim_index);
            count += 1;
        }
        // Write last pending tile
        if count > 0 {
            self.offset_and_count.push(OffsetAndCount {
                offset: offset as u32,
                count,
            });
        }
        // Write empty tile(s) at the end
        for _ in ti + 1..oc_extra as i32 {
            self.offset_and_count.push(OffsetAndCount {
                offset: offset as u32,
                count: 0,
            });
        }

        // Clear scratch buffer for next call
        self.assigned_tiles.clear();
    }
}

/// Ensure any active [`Camera`] component with a [`Canvas`] component also has
/// associated [`TileConfig`] and [`Tiles`] components.
pub fn spawn_missing_tiles_components(
    mut commands: Commands,
    cameras: Query<(Entity, Option<&TileConfig>, &Camera), (With<Canvas>, Without<Tiles>)>,
) {
    for (entity, config, camera) in &cameras {
        if !camera.is_active {
            continue;
        }

        let config = config.copied().unwrap_or_default();
        commands.entity(entity).insert((Tiles::default(), config));
    }
}

pub fn resize_tiles_to_camera_render_target(
    mut views: Query<(&Camera, &TileConfig, &mut Tiles), With<Canvas>>,
) {
    // Loop on all camera views
    for (camera, _tile_config, tiles) in &mut views {
        let Some(screen_size) = camera.physical_viewport_size() else {
            continue;
        };

        // Resize tile storage to fit the viewport size
        let tiles = tiles.into_inner();
        tiles.update_size(screen_size);
    }
}

pub fn allocate_atlas_layouts(
    mut query: Query<&mut Canvas>,
    mut layouts: ResMut<Assets<TextureAtlasLayout>>,
) {
    for mut canvas in query.iter_mut() {
        // FIXME
        let size = UVec2::splat(1024);

        // FIXME - also check for resize...
        if canvas.atlas_layout == Handle::<TextureAtlasLayout>::default() {
            canvas.atlas_layout = layouts.add(TextureAtlasLayout::new_empty(size));
        }
    }
}

/// Calculate the width of a fixed-aspect rectangle given a content height.
fn aspect_width(size: Vec2, content_height: f32) -> f32 {
    size.x.max(0.) / size.y.max(1.) * content_height.max(0.)
}

/// Calculate the size of a rectangle such that its width fits in the given
/// content, and its height either stretches to that content or it keeps its
/// aspect ratio (and therefore gets clipped).
fn fit_width(size: Vec2, content_size: Vec2, stretch_height: bool) -> Vec2 {
    Vec2::new(
        content_size.x,
        if stretch_height {
            content_size.y
        } else {
            aspect_height(size, content_size.x)
        },
    )
}

/// Calculate the height of a fixed-aspect rectangle given a content width.
fn aspect_height(size: Vec2, content_width: f32) -> f32 {
    size.y.max(0.) / size.x.max(1.) * content_width.max(0.)
}

/// Calculate the size of a rectangle such that its height fits in the given
/// content, and its width either stretches to that content or it keeps its
/// aspect ratio (and therefore gets clipped).
fn fit_height(size: Vec2, content_size: Vec2, stretch_width: bool) -> Vec2 {
    Vec2::new(
        if stretch_width {
            content_size.x
        } else {
            aspect_width(size, content_size.y)
        },
        content_size.y,
    )
}

/// Calculate the size of a rectangle such that both its width and height fit in
/// the given content, and the other direction either stretches to that content
/// or it keeps its aspect ratio. This ensures the returned rectangle covers the
/// content, possibly clipping in one direction to do so.
fn fit_any(size: Vec2, content_size: Vec2, stretch_other: bool) -> Vec2 {
    let aspect = size.x.max(0.) / size.y.max(1.);
    let content_aspect = content_size.x.max(0.) / content_size.y.max(1.);
    if aspect >= content_aspect {
        fit_height(size, content_size, stretch_other)
    } else {
        fit_width(size, content_size, stretch_other)
    }
}

/// Process all images drawn onto all canvases.
///
/// This calculates the proper image size given the content rectangle size and
/// the window scale factor, applying any image scaling as specified during the
/// draw call.
pub fn process_images(
    images: Res<Assets<Image>>,
    q_window: Query<&Window, With<PrimaryWindow>>,
    mut q_canvas: Query<&mut Canvas>,
) {
    // TODO - handle multi-window
    let Ok(primary_window) = q_window.get_single() else {
        return;
    };
    let scale_factor = primary_window.scale_factor() as f32;

    for mut canvas in q_canvas.iter_mut() {
        for prim in &mut canvas.primitives {
            let Primitive::Rect(rect) = prim else {
                continue;
            };
            let Some(id) = rect.image else {
                continue;
            };
            if let Some(image) = images.get(id) {
                let image_size = Vec2::new(
                    image.texture_descriptor.size.width as f32,
                    image.texture_descriptor.size.height as f32,
                );
                let content_size = rect.rect.size() * scale_factor;
                rect.image_size = match rect.image_scaling {
                    ImageScaling::Uniform(ratio) => image_size * ratio,
                    ImageScaling::FitWidth(stretch_height) => {
                        fit_width(image_size, content_size, stretch_height)
                    }
                    ImageScaling::FitHeight(stretch_width) => {
                        fit_height(image_size, content_size, stretch_width)
                    }
                    ImageScaling::Fit(stretch_other) => {
                        fit_any(image_size, content_size, stretch_other)
                    }
                    ImageScaling::Stretch => content_size,
                }
            } else {
                warn!("Unknown image asset ID {:?}; skipped.", id);
                rect.image = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles() {
        let mut tiles = Tiles::default();
        tiles.update_size(UVec2::new(32, 64));
        assert_eq!(tiles.dimensions, UVec2::new(4, 8));
        assert!(tiles.primitives.is_empty());
        assert!(tiles.offset_and_count.is_empty());
        assert_eq!(tiles.offset_and_count.capacity(), 32);

        let prim_index = PackedPrimitiveIndex::new(42, GpuPrimitiveKind::Line, true, false);
        tiles.assign_to_tiles(
            &[PreparedPrimitive {
                // 8 x 16, exactly aligned on the tile grid => 2 tiles exactly
                aabb: Aabb2d {
                    min: Vec2::new(8., 16.),
                    max: Vec2::new(16., 32.),
                },
                prim_index,
            }],
            // Large screen size, no effect in this test
            Vec2::new(256., 128.),
        );

        assert_eq!(tiles.primitives.len(), 2);
        assert_eq!(tiles.primitives[0], prim_index);
        assert_eq!(tiles.primitives[1], prim_index);

        assert_eq!(tiles.offset_and_count.len(), 32);
        for (idx, oc) in tiles.offset_and_count.iter().enumerate() {
            if idx == 9 || idx == 13 {
                assert_eq!(oc.count, 1);
                assert_eq!(oc.offset, if idx == 9 { 0 } else { 1 });
            } else {
                assert_eq!(oc.count, 0);
            }
        }
    }

    #[test]
    fn aspect() {
        // Aspect ratios
        assert_eq!(aspect_width(Vec2::ZERO, 0.), 0.);
        assert_eq!(aspect_height(Vec2::ZERO, 0.), 0.);
        assert_eq!(aspect_width(Vec2::ZERO, 1.), 0.);
        assert_eq!(aspect_height(Vec2::ZERO, 1.), 0.);
        assert_eq!(aspect_width(Vec2::ONE, 0.), 0.);
        assert_eq!(aspect_height(Vec2::ONE, 0.), 0.);

        // Expand to fit
        assert_eq!(aspect_width(Vec2::new(256., 64.), 128.), 512.);
        assert_eq!(aspect_height(Vec2::new(256., 64.), 512.), 128.);

        // Shrink to fit
        assert_eq!(aspect_width(Vec2::new(256., 128.), 64.), 128.);
        assert_eq!(aspect_height(Vec2::new(256., 64.), 128.), 32.);
    }

    #[test]
    fn fit() {
        // Fit to zero-sized content is always zero
        assert_eq!(fit_width(Vec2::ZERO, Vec2::ZERO, false), Vec2::ZERO);
        assert_eq!(fit_height(Vec2::ZERO, Vec2::ZERO, false), Vec2::ZERO);
        assert_eq!(fit_any(Vec2::ZERO, Vec2::ZERO, false), Vec2::ZERO);
        assert_eq!(fit_width(Vec2::ONE, Vec2::ZERO, false), Vec2::ZERO);
        assert_eq!(fit_height(Vec2::ONE, Vec2::ZERO, false), Vec2::ZERO);
        assert_eq!(fit_any(Vec2::ONE, Vec2::ZERO, false), Vec2::ZERO);
        assert_eq!(fit_width(Vec2::ZERO, Vec2::ZERO, true), Vec2::ZERO);
        assert_eq!(fit_height(Vec2::ZERO, Vec2::ZERO, true), Vec2::ZERO);
        assert_eq!(fit_any(Vec2::ZERO, Vec2::ZERO, true), Vec2::ZERO);
        assert_eq!(fit_width(Vec2::ONE, Vec2::ZERO, true), Vec2::ZERO);
        assert_eq!(fit_height(Vec2::ONE, Vec2::ZERO, true), Vec2::ZERO);
        assert_eq!(fit_any(Vec2::ONE, Vec2::ZERO, true), Vec2::ZERO);

        // Fit zero-sized (size is ignored in fit direction, only content matters)
        assert_eq!(fit_width(Vec2::ZERO, Vec2::ONE, false), Vec2::X);
        assert_eq!(fit_height(Vec2::ZERO, Vec2::ONE, false), Vec2::Y);
        assert_eq!(fit_width(Vec2::ZERO, Vec2::ONE, true), Vec2::ONE);
        assert_eq!(fit_height(Vec2::ZERO, Vec2::ONE, true), Vec2::ONE);

        // Expand to fit
        assert_eq!(
            fit_width(Vec2::new(256., 64.), Vec2::new(512., 32.), false),
            Vec2::new(512., 128.)
        );
        assert_eq!(
            fit_height(Vec2::new(256., 64.), Vec2::new(128., 128.), false),
            Vec2::new(512., 128.)
        );
        assert_eq!(
            fit_width(Vec2::new(256., 64.), Vec2::new(512., 32.), true),
            Vec2::new(512., 32.)
        );
        assert_eq!(
            fit_height(Vec2::new(256., 64.), Vec2::new(128., 128.), true),
            Vec2::new(128., 128.)
        );

        // Shrink to fit
        assert_eq!(
            fit_width(Vec2::new(256., 64.), Vec2::new(128., 128.), false),
            Vec2::new(128., 32.)
        );
        assert_eq!(
            fit_height(Vec2::new(256., 64.), Vec2::new(512., 32.), false),
            Vec2::new(128., 32.)
        );
        assert_eq!(
            fit_width(Vec2::new(256., 64.), Vec2::new(128., 128.), true),
            Vec2::new(128., 128.)
        );
        assert_eq!(
            fit_height(Vec2::new(256., 64.), Vec2::new(512., 32.), true),
            Vec2::new(512., 32.)
        );
    }
}
