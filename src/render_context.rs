//! Rendering context exposing convenience functions to draw into a [`Canvas`].

use std::str;

//use bevy::math::Affine2;
use bevy::math::{Rect, Vec2};
use bevy::prelude::*;
use bevy::sprite::Anchor;
use bevy::text::TextLayoutInfo;

use crate::{
    canvas::{Canvas, LinePrimitive, RectPrimitive, TextPrimitive},
    shapes::Shape,
    ShapeRef,
};

/// Abstraction of a brush to draw shapes.
///
/// Currently only support solid colors (no pattern or gradient yet).
#[derive(Debug, Clone)]
pub struct Brush {
    color: Color,
}

impl Default for Brush {
    fn default() -> Self {
        Self {
            color: Color::BLACK,
        }
    }
}

impl From<Color> for Brush {
    fn from(color: Color) -> Self {
        Self { color }
    }
}

impl From<&Color> for Brush {
    fn from(color: &Color) -> Self {
        Self { color: *color }
    }
}

impl Brush {
    /// Get the brush color.
    pub fn color(&self) -> Color {
        self.color.clone()
    }
}

// impl<'c> IntoBrush<RenderContext<'c>> for Brush {
//     fn make_brush<'b>(
//         &'b self,
//         _piet: &mut RenderContext,
//         _bbox: impl FnOnce() -> KRect,
//     ) -> std::borrow::Cow<'b, Brush> {
//         std::borrow::Cow::Borrowed(self)
//     }
// }

pub trait TextStorage: 'static {
    fn as_str(&self) -> &str;
}

impl TextStorage for String {
    fn as_str(&self) -> &str {
        &self[..]
    }
}

impl TextStorage for &'static str {
    fn as_str(&self) -> &str {
        self
    }
}

// #[derive(Debug)]
// pub struct Text<'c> {
//     layouts: &'c Vec<TextLayout>,
// }

/// Layout of a single text.
///
/// This is generated by [`RenderContext::new_layout()`].
#[derive(Debug, Clone)]
pub struct TextLayout {
    /// Unique ID of the text into its owner [`Canvas`].
    pub(crate) id: u32,
    /// Sections of text.
    pub(crate) sections: Vec<TextSection>,
    /// Text anchor defining the position of the text relative to its bounding
    /// rectangle, if any.
    pub(crate) anchor: Anchor,
    /// Text justifying. This only affects multiline text.
    pub(crate) justify: JustifyText,
    /// Text bounds, used for glyph clipping.
    pub(crate) bounds: Vec2,
    /// Calculated text size based on glyphs alone, updated by
    /// [`process_glyphs()`].
    pub(crate) calculated_size: Vec2,
    /// Layout info calculated by the [`KeithTextPipeline`] during
    /// [`process_glyphs()`].
    pub(crate) layout_info: Option<TextLayoutInfo>,
}

impl Default for TextLayout {
    fn default() -> Self {
        Self {
            id: 0,
            sections: vec![],
            anchor: Anchor::default(),
            justify: JustifyText::Left,
            bounds: Vec2::ZERO,
            calculated_size: Vec2::ZERO,
            layout_info: None,
        }
    }
}

pub struct TextLayoutBuilder<'c> {
    canvas: &'c mut Canvas,
    style: TextStyle,
    value: String,
    bounds: Vec2,
    anchor: Anchor,
    alignment: JustifyText,
}

impl<'c> TextLayoutBuilder<'c> {
    fn new(canvas: &'c mut Canvas, storage: impl TextStorage) -> Self {
        Self {
            canvas,
            style: TextStyle::default(),
            value: storage.as_str().to_owned(),
            bounds: Vec2::new(f32::MAX, f32::MAX),
            anchor: Anchor::default(),
            alignment: JustifyText::Left, // Bottom,
        }
    }

    /// Select the font to render the text with.
    pub fn font(mut self, font: Handle<Font>) -> Self {
        self.style.font = font;
        self
    }

    /// Set the font size.
    pub fn font_size(mut self, font_size: f32) -> Self {
        self.style.font_size = font_size;
        self
    }

    /// Set the text color.
    ///
    /// FIXME - this vs. RenderContext::draw_text()'s color
    pub fn color(mut self, color: Color) -> Self {
        self.style.color = color;
        self
    }

    /// Set some bounds around the text.
    ///
    /// The text will be formatted with line wrapping and clipped to fit in
    /// those bounds.
    ///
    /// FIXME - Currently no clipping for partially visible glyphs, only
    /// completely outside ones are clipped.
    pub fn bounds(mut self, bounds: Vec2) -> Self {
        self.bounds = bounds;
        self
    }

    /// Set the text anchor point.
    pub fn anchor(mut self, anchor: Anchor) -> Self {
        self.anchor = anchor;
        self
    }

    /// Set the text alignment relative to its render position.
    pub fn alignment(mut self, alignment: JustifyText) -> Self {
        self.alignment = alignment;
        self
    }

    /// Finalize the layout building and return the newly allocated text layout
    /// ID.
    ///
    /// FIXME - Return CanvasTextId somehow, to ensure texts are not used
    /// cross-Canvas.
    pub fn build(self) -> u32 {
        let layout = TextLayout {
            id: 0, // assigned in finish_layout()
            sections: vec![TextSection {
                style: self.style,
                value: self.value,
            }],
            anchor: self.anchor,
            justify: self.alignment,
            bounds: self.bounds,
            calculated_size: Vec2::ZERO, // updated in process_glyphs()
            layout_info: None,
        };
        self.canvas.finish_layout(layout)
    }
}

// #[derive(Debug, Default, Clone)]
// pub struct BevyImage {
//     image: bevy::render::texture::Image,
// }

// impl BevyImage {
//     fn new(width: usize, height: usize, buf: &[u8], format:
// piet::ImageFormat) -> Self {         let data = buf.to_vec();
//         let format = match format {
//             piet::ImageFormat::Grayscale =>
// bevy::render::render_resource::TextureFormat::R8Unorm,
// piet::ImageFormat::Rgb => unimplemented!(),
// piet::ImageFormat::RgbaSeparate => {
// bevy::render::render_resource::TextureFormat::Rgba8Unorm             }
//             piet::ImageFormat::RgbaPremul => unimplemented!(),
//             _ => unimplemented!(),
//         };
//         let image = bevy::render::texture::Image::new(
//             bevy::render::render_resource::Extent3d {
//                 width: width as u32,
//                 height: height as u32,
//                 depth_or_array_layers: 1,
//             },
//             bevy::render::render_resource::TextureDimension::D2,
//             data,
//             format,
//         );
//         Self { image }
//     }
// }

/// Scaling for image rendering.
#[derive(Debug, Clone, Copy)]
pub enum ImageScaling {
    /// Scale the image uniformly by the given factor.
    ///
    /// This is the default, with `factor == 1.`, and draws the image at its
    /// native size. Values greater than `1.` increase the image size (zoom in),
    /// while values less than `1.` decrase it (zoom out).
    Uniform(f32),
    /// Fit the image width to the target content width.
    ///
    /// If `true`, stretch the height; otherwise keep the aspect ratio and crop
    /// it.
    FitWidth(bool),
    /// Fit the image height to the target content height.
    ///
    /// If `true`, stretch the width; otherwise keep the aspect ratio and crop
    /// it.
    FitHeight(bool),
    /// Fit either the image width or height to the target content size, such
    /// that it covers the content.
    ///
    /// If `true`, stretch the other direction; otherwise crop it.
    Fit(bool),
    /// Stretch the image to fit exactly the target content size.
    Stretch,
}

impl Default for ImageScaling {
    fn default() -> Self {
        Self::Uniform(1.0)
    }
}

/// Rendering context providing a higher level API to draw on a [`Canvas`].
pub struct RenderContext<'c> {
    /// Transform applied to all operations on this render context.
    //transform: Affine2,
    /// Underlying canvas render operations are directed to.
    canvas: &'c mut Canvas,
}

impl<'c> RenderContext<'c> {
    /// Create a new render context to draw on an existing canvas.
    pub fn new(canvas: &'c mut Canvas) -> Self {
        Self {
            //transform: Affine2::IDENTITY, // FIXME - unused
            canvas,
        }
    }

    /// Create a solid-color brush.
    pub fn solid_brush(&mut self, color: Color) -> Brush {
        color.into()
    }

    /// Clear an area of the render context with a specific color.
    ///
    /// To clear the entire underlying canvas, prefer using [`Canvas::clear()`].
    pub fn clear(&mut self, region: Option<Rect>, color: Color) {
        if let Some(rect) = region {
            // TODO - delete primitives covered by region
            self.fill(rect, &Brush { color });
        } else {
            self.canvas.clear();
            self.fill(self.canvas.rect(), &Brush { color });
        }
    }

    /// Fill a shape with a given brush.
    pub fn fill(&mut self, shape: impl Shape, brush: &Brush) -> ShapeRef {
        shape.fill(self.canvas, brush)
    }

    // Stroke a shape with a given brush.
    // pub fn stroke(&mut self, shape: impl Shape, brush: &Brush, thickness: f32) {
    //     shape.stroke(self.canvas, brush, thickness);
    // }

    /// Draw a line between two points with the given brush.
    ///
    /// The line thickness is centered on the mathematical line between the two
    /// endpoints, spanning `thickness / 2.` on each side.
    pub fn line(&mut self, p0: Vec2, p1: Vec2, brush: &Brush, thickness: f32) -> ShapeRef {
        self.canvas.draw(LinePrimitive {
            start: p0,
            end: p1,
            color: brush.color(),
            thickness,
            ..default()
        })
    }

    /// Create a new text layout to draw a text.
    ///
    /// See [`draw_text()`] for details.
    ///
    /// [`draw_text()`]: RenderContext::draw_text
    pub fn new_layout(&mut self, text: impl TextStorage) -> TextLayoutBuilder {
        TextLayoutBuilder::new(self.canvas, text)
    }

    /// Draw a text created by a [`TextLayoutBuilder`].
    ///
    /// To draw a text, first call [`new_layout()`] to define a text with its
    /// layout and styles. Then pass the handle returned by
    /// [`TextLayoutBuilder::build()`] to this function.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use bevy_keith::*;
    /// # use bevy::{prelude::*, color::palettes::css::*};
    /// # let mut canvas = Canvas::default();
    /// # let mut ctx = RenderContext::new(&mut canvas);
    /// # let rect = Rect::new(0., 0., 1., 1.);
    /// # let brush = ctx.solid_brush(RED.into());
    /// # let font: Handle<Font> = unimplemented!();
    /// let text = ctx
    ///     .new_layout("Hello world!")
    ///     .color(Color::srgb(1., 1., 1.))
    ///     .font(font.clone())
    ///     .font_size(16.)
    ///     .alignment(JustifyText::Center)
    ///     .build();
    /// ctx.draw_text(text, Vec2::new(100., 20.));
    /// ```
    ///
    /// [`new_layout()`]: RenderContext::new_layout
    pub fn draw_text(&mut self, text_id: u32, pos: Vec2) {
        self.canvas.draw(TextPrimitive {
            id: text_id,
            rect: Rect { min: pos, max: pos },
        });
    }

    /// Draw an image inside a given rectangle.
    ///
    /// The image is drawn inside the given rectangle shape, centered on it and
    /// scaled according to the given [`ImageScaling`].
    pub fn draw_image(&mut self, shape: Rect, image: Handle<Image>, scaling: ImageScaling) {
        self.canvas.draw(RectPrimitive {
            rect: shape,
            color: Color::WHITE,
            image: Some(image.id()),
            image_scaling: scaling,
            ..Default::default()
        });
    }
}

impl<'c> Drop for RenderContext<'c> {
    fn drop(&mut self) {
        self.canvas.finish();
    }
}
