use super::utilsprites::RenderMetrics;
use ::window::bitmaps::atlas::{Atlas, Sprite};
#[cfg(test)]
use ::window::bitmaps::ImageTexture;
use ::window::bitmaps::{BitmapImage, Image, Texture2d};
use ::window::color::{LinearRgba, SrgbaPixel};
use ::window::glium;
use ::window::glium::backend::Context as GliumContext;
use ::window::glium::texture::SrgbTexture2d;
use ::window::{Point, Rect};
use anyhow::{anyhow, Context};
use config::{configuration, AllowSquareGlyphOverflow, TextStyle};
use euclid::{num::Zero, Box2D};
use lru::LruCache;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use termwiz::image::ImageData;
use wezterm_font::units::*;
use wezterm_font::{FontConfiguration, GlyphInfo};
use wezterm_term::Underline;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    pub font_idx: usize,
    pub glyph_pos: u32,
    pub style: TextStyle,
    pub followed_by_space: bool,
}

/// We'd like to avoid allocating when resolving from the cache
/// so this is the borrowed version of GlyphKey.
/// It's a bit involved to make this work; more details can be
/// found in the excellent guide here:
/// <https://github.com/sunshowers/borrow-complex-key-example/blob/master/src/lib.rs>
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct BorrowedGlyphKey<'a> {
    pub font_idx: usize,
    pub glyph_pos: u32,
    pub style: &'a TextStyle,
    pub followed_by_space: bool,
}

impl<'a> BorrowedGlyphKey<'a> {
    fn to_owned(&self) -> GlyphKey {
        GlyphKey {
            font_idx: self.font_idx,
            glyph_pos: self.glyph_pos,
            style: self.style.clone(),
            followed_by_space: self.followed_by_space,
        }
    }
}

trait GlyphKeyTrait {
    fn key<'k>(&'k self) -> BorrowedGlyphKey<'k>;
}

impl GlyphKeyTrait for GlyphKey {
    fn key<'k>(&'k self) -> BorrowedGlyphKey<'k> {
        BorrowedGlyphKey {
            font_idx: self.font_idx,
            glyph_pos: self.glyph_pos,
            style: &self.style,
            followed_by_space: self.followed_by_space,
        }
    }
}

impl<'a> GlyphKeyTrait for BorrowedGlyphKey<'a> {
    fn key<'k>(&'k self) -> BorrowedGlyphKey<'k> {
        *self
    }
}

impl<'a> std::borrow::Borrow<dyn GlyphKeyTrait + 'a> for GlyphKey {
    fn borrow(&self) -> &(dyn GlyphKeyTrait + 'a) {
        self
    }
}

impl<'a> PartialEq for (dyn GlyphKeyTrait + 'a) {
    fn eq(&self, other: &Self) -> bool {
        self.key().eq(&other.key())
    }
}

impl<'a> Eq for (dyn GlyphKeyTrait + 'a) {}

impl<'a> std::hash::Hash for (dyn GlyphKeyTrait + 'a) {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key().hash(state)
    }
}

/// Caches a rendered glyph.
/// The image data may be None for whitespace glyphs.
pub struct CachedGlyph<T: Texture2d> {
    pub has_color: bool,
    pub x_offset: PixelLength,
    pub y_offset: PixelLength,
    pub bearing_x: PixelLength,
    pub bearing_y: PixelLength,
    pub texture: Option<Sprite<T>>,
    pub scale: f64,
}

impl<T: Texture2d> std::fmt::Debug for CachedGlyph<T> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::result::Result<(), std::fmt::Error> {
        fmt.debug_struct("CachedGlyph")
            .field("has_color", &self.has_color)
            .field("x_offset", &self.x_offset)
            .field("y_offset", &self.y_offset)
            .field("bearing_x", &self.bearing_x)
            .field("bearing_y", &self.bearing_y)
            .field("scale", &self.scale)
            .field("texture", &self.texture)
            .finish()
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
struct LineKey {
    strike_through: bool,
    underline: Underline,
    overline: bool,
}

bitflags::bitflags! {
    pub struct Quadrant: u8{
        const UPPER_LEFT = 1<<1;
        const UPPER_RIGHT = 1<<2;
        const LOWER_LEFT = 1<<3;
        const LOWER_RIGHT = 1<<4;
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum BlockAlpha {
    /// 100%
    Full,
    /// 75%
    Dark,
    /// 50%
    Medium,
    /// 25%
    Light,
}

/// Represents glyphs that require custom drawing logic.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum CustomGlyphKey {
    /// Represents a Box Drawing glyph, decoded from
    /// <https://en.wikipedia.org/wiki/Box_Drawing_(Unicode_block)>
    /// <https://www.unicode.org/charts/PDF/U2500.pdf>
    BoxDrawing(BoxDrawingKey),

    /// Represents a Block Element glyph, decoded from
    /// <https://en.wikipedia.org/wiki/Block_Elements>
    /// <https://www.unicode.org/charts/PDF/U2580.pdf>
    Block(BlockKey),
}

impl CustomGlyphKey {
    pub fn from_char(c: char) -> Option<Self> {
        let n = c as u32;
        match n {
            0x2500..=0x257f => BoxDrawingKey::from_char(c).map(Self::BoxDrawing),
            0x2580..=0x259f => BlockKey::from_char(c).map(Self::Block),
            _ => None,
        }
    }

    pub fn from_cell(cell: &termwiz::cell::Cell) -> Option<Self> {
        let mut chars = cell.str().chars();
        let first_char = chars.next()?;
        if chars.next().is_some() {
            None
        } else {
            Self::from_char(first_char)
        }
    }
}

/// Represents a Block Element glyph, decoded from
/// <https://en.wikipedia.org/wiki/Block_Elements>
/// <https://www.unicode.org/charts/PDF/U2580.pdf>
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum BlockKey {
    /// Number of 1/8ths in the upper half
    Upper(u8),
    /// Number of 1/8ths in the lower half
    Lower(u8),
    /// Number of 1/8ths in the left half
    Left(u8),
    /// Number of 1/8ths in the right half
    Right(u8),
    /// Full block with alpha level
    Full(BlockAlpha),
    /// A combination of quadrants
    Quadrants(Quadrant),
}

impl BlockKey {
    pub fn from_char(c: char) -> Option<Self> {
        let c = c as u32;
        Some(match c {
            // Upper half block
            0x2580 => Self::Upper(4),
            // Lower 1..7 eighths
            0x2581..=0x2587 => Self::Lower((c - 0x2580) as u8),
            0x2588 => Self::Full(BlockAlpha::Full),
            // Left 7..1 eighths
            0x2589..=0x258f => Self::Left((0x2590 - c) as u8),
            // Right half
            0x2590 => Self::Right(4),
            0x2591 => Self::Full(BlockAlpha::Light),
            0x2592 => Self::Full(BlockAlpha::Medium),
            0x2593 => Self::Full(BlockAlpha::Dark),
            0x2594 => Self::Upper(1),
            0x2595 => Self::Right(1),
            0x2596 => Self::Quadrants(Quadrant::LOWER_LEFT),
            0x2597 => Self::Quadrants(Quadrant::LOWER_RIGHT),
            0x2598 => Self::Quadrants(Quadrant::UPPER_LEFT),
            0x2599 => {
                Self::Quadrants(Quadrant::UPPER_LEFT | Quadrant::LOWER_LEFT | Quadrant::LOWER_RIGHT)
            }
            0x259a => Self::Quadrants(Quadrant::UPPER_LEFT | Quadrant::LOWER_RIGHT),
            0x259b => {
                Self::Quadrants(Quadrant::UPPER_LEFT | Quadrant::UPPER_RIGHT | Quadrant::LOWER_LEFT)
            }
            0x259c => Self::Quadrants(
                Quadrant::UPPER_LEFT | Quadrant::UPPER_RIGHT | Quadrant::LOWER_RIGHT,
            ),
            0x259d => Self::Quadrants(Quadrant::UPPER_RIGHT),
            0x259e => Self::Quadrants(Quadrant::UPPER_RIGHT | Quadrant::LOWER_LEFT),
            0x259f => Self::Quadrants(
                Quadrant::UPPER_RIGHT | Quadrant::LOWER_LEFT | Quadrant::LOWER_RIGHT,
            ),
            _ => return None,
        })
    }
}

/// Represents a Box Drawing glyph, decoded from
/// <https://en.wikipedia.org/wiki/Box_Drawing_(Unicode_block)>
/// <https://www.unicode.org/charts/PDF/U2500.pdf>
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum BoxDrawingKey {
    LightHorizontal,
    HeavyHorizontal,
    LightVertical,
    HeavyVertical,
}

impl BoxDrawingKey {
    pub fn from_char(c: char) -> Option<Self> {
        use BoxDrawingKey::*;

        let c = c as u32;
        Some(match c {
            0x2500 => LightHorizontal,
            0x2501 => HeavyHorizontal,
            0x2502 => LightVertical,
            0x2503 => HeavyVertical,
            _ => return None,
        })
    }
}

#[derive(Debug)]
pub struct ImageFrame {
    duration: Duration,
    image: ::window::bitmaps::Image,
}

#[derive(Debug)]
pub enum CachedImage {
    Animation(DecodedImage),
    SingleFrame,
}

#[derive(Debug)]
pub struct DecodedImage {
    frame_start: Instant,
    current_frame: usize,
    frames: Vec<ImageFrame>,
}

impl DecodedImage {
    fn placeholder() -> Self {
        let image = ::window::bitmaps::Image::new(1, 1);
        let frame = ImageFrame {
            duration: Duration::default(),
            image,
        };
        Self {
            frame_start: Instant::now(),
            current_frame: 0,
            frames: vec![frame],
        }
    }

    fn with_frames(frames: Vec<image::Frame>) -> Self {
        let frames = frames
            .into_iter()
            .map(|frame| {
                let duration: Duration = frame.delay().into();
                let image = image::DynamicImage::ImageRgba8(frame.into_buffer()).to_rgba8();
                let (w, h) = image.dimensions();
                let width = w as usize;
                let height = h as usize;
                let image = ::window::bitmaps::Image::from_raw(width, height, image.into_vec());
                ImageFrame { duration, image }
            })
            .collect();
        Self {
            frame_start: Instant::now(),
            current_frame: 0,
            frames,
        }
    }

    fn with_single(image_data: &Arc<ImageData>) -> anyhow::Result<Self> {
        let image = image::load_from_memory(image_data.data())?.to_rgba8();
        let (width, height) = image.dimensions();
        let width = width as usize;
        let height = height as usize;
        let image = ::window::bitmaps::Image::from_raw(width, height, image.into_vec());
        Ok(Self {
            frame_start: Instant::now(),
            current_frame: 0,
            frames: vec![ImageFrame {
                duration: Default::default(),
                image,
            }],
        })
    }

    fn load(image_data: &Arc<ImageData>) -> anyhow::Result<Self> {
        use image::{AnimationDecoder, ImageFormat};
        let format = image::guess_format(image_data.data())?;
        match format {
            ImageFormat::Gif => image::gif::GifDecoder::new(image_data.data())
                .and_then(|decoder| decoder.into_frames().collect_frames())
                .and_then(|frames| Ok(Self::with_frames(frames)))
                .or_else(|err| {
                    log::error!(
                        "Unable to parse animated gif: {:#}, trying as single frame",
                        err
                    );
                    Self::with_single(image_data)
                }),
            ImageFormat::Png => {
                let decoder = image::png::PngDecoder::new(image_data.data())?;
                if decoder.is_apng() {
                    let frames = decoder.apng().into_frames().collect_frames()?;
                    Ok(Self::with_frames(frames))
                } else {
                    Self::with_single(image_data)
                }
            }
            _ => Self::with_single(image_data),
        }
    }
}

pub struct GlyphCache<T: Texture2d> {
    glyph_cache: HashMap<GlyphKey, Rc<CachedGlyph<T>>>,
    pub atlas: Atlas<T>,
    fonts: Rc<FontConfiguration>,
    pub image_cache: LruCache<usize, CachedImage>,
    frame_cache: HashMap<(usize, usize), Sprite<T>>,
    line_glyphs: HashMap<LineKey, Sprite<T>>,
    custom_glyphs: HashMap<CustomGlyphKey, Sprite<T>>,
    metrics: RenderMetrics,
}

#[cfg(test)]
impl GlyphCache<ImageTexture> {
    pub fn new_in_memory(
        fonts: &Rc<FontConfiguration>,
        size: usize,
        metrics: &RenderMetrics,
    ) -> anyhow::Result<Self> {
        let surface = Rc::new(ImageTexture::new(size, size));
        let atlas = Atlas::new(&surface).expect("failed to create new texture atlas");

        Ok(Self {
            fonts: Rc::clone(fonts),
            glyph_cache: HashMap::new(),
            image_cache: LruCache::new(16),
            frame_cache: HashMap::new(),
            atlas,
            metrics: metrics.clone(),
            line_glyphs: HashMap::new(),
            custom_glyphs: HashMap::new(),
        })
    }
}

impl GlyphCache<SrgbTexture2d> {
    pub fn new_gl(
        backend: &Rc<GliumContext>,
        fonts: &Rc<FontConfiguration>,
        size: usize,
        metrics: &RenderMetrics,
    ) -> anyhow::Result<Self> {
        let surface = Rc::new(SrgbTexture2d::empty_with_format(
            backend,
            glium::texture::SrgbFormat::U8U8U8U8,
            glium::texture::MipmapsOption::NoMipmap,
            size as u32,
            size as u32,
        )?);
        let atlas = Atlas::new(&surface).expect("failed to create new texture atlas");

        Ok(Self {
            fonts: Rc::clone(fonts),
            glyph_cache: HashMap::new(),
            image_cache: LruCache::new(16),
            frame_cache: HashMap::new(),
            atlas,
            metrics: metrics.clone(),
            line_glyphs: HashMap::new(),
            custom_glyphs: HashMap::new(),
        })
    }

    pub fn clear(&mut self) {
        self.atlas.clear();
        // self.image_cache.clear(); - relatively expensive to re-populate
        self.frame_cache.clear();
        self.glyph_cache.clear();
        self.line_glyphs.clear();
        self.custom_glyphs.clear();
    }
}

impl<T: Texture2d> GlyphCache<T> {
    /// Resolve a glyph from the cache, rendering the glyph on-demand if
    /// the cache doesn't already hold the desired glyph.
    pub fn cached_glyph(
        &mut self,
        info: &GlyphInfo,
        style: &TextStyle,
        followed_by_space: bool,
    ) -> anyhow::Result<Rc<CachedGlyph<T>>> {
        let key = BorrowedGlyphKey {
            font_idx: info.font_idx,
            glyph_pos: info.glyph_pos,
            style,
            followed_by_space,
        };

        if let Some(entry) = self.glyph_cache.get(&key as &dyn GlyphKeyTrait) {
            return Ok(Rc::clone(entry));
        }

        let glyph = self
            .load_glyph(info, style, followed_by_space)
            .with_context(|| anyhow!("load_glyph {:?} {:?}", info, style))?;
        self.glyph_cache.insert(key.to_owned(), Rc::clone(&glyph));
        Ok(glyph)
    }

    /// Perform the load and render of a glyph
    #[allow(clippy::float_cmp)]
    fn load_glyph(
        &mut self,
        info: &GlyphInfo,
        style: &TextStyle,
        followed_by_space: bool,
    ) -> anyhow::Result<Rc<CachedGlyph<T>>> {
        let base_metrics;
        let idx_metrics;
        let glyph;

        {
            let font = self.fonts.resolve_font(style)?;
            base_metrics = font.metrics();
            glyph = font.rasterize_glyph(info.glyph_pos, info.font_idx)?;

            idx_metrics = font.metrics_for_idx(info.font_idx)?;
        }

        let y_scale = base_metrics.cell_height.get() / idx_metrics.cell_height.get();
        let x_scale =
            base_metrics.cell_width.get() / (idx_metrics.cell_width.get() / info.num_cells as f64);

        let aspect = (idx_metrics.cell_height / idx_metrics.cell_width).get();
        let is_square_or_wide = aspect >= 0.9;

        let allow_width_overflow = if is_square_or_wide {
            match configuration().allow_square_glyphs_to_overflow_width {
                AllowSquareGlyphOverflow::Never => false,
                AllowSquareGlyphOverflow::Always => true,
                AllowSquareGlyphOverflow::WhenFollowedBySpace => followed_by_space,
            }
        } else {
            false
        };

        let scale = if !allow_width_overflow
            && y_scale * glyph.width as f64 > base_metrics.cell_width.get() * info.num_cells as f64
        {
            // y-scaling would make us too wide, so use the x-scale
            x_scale
        } else {
            y_scale
        };

        let (cell_width, cell_height) = (base_metrics.cell_width, base_metrics.cell_height);

        let glyph = if glyph.width == 0 || glyph.height == 0 {
            // a whitespace glyph
            CachedGlyph {
                has_color: glyph.has_color,
                texture: None,
                x_offset: info.x_offset * scale,
                y_offset: info.y_offset * scale,
                bearing_x: PixelLength::zero(),
                bearing_y: PixelLength::zero(),
                scale,
            }
        } else {
            let raw_im = Image::with_rgba32(
                glyph.width as usize,
                glyph.height as usize,
                4 * glyph.width as usize,
                &glyph.data,
            );

            let bearing_x = glyph.bearing_x * scale;
            let bearing_y = glyph.bearing_y * scale;
            let x_offset = info.x_offset * scale;
            let y_offset = info.y_offset * scale;

            let (scale, raw_im) = if scale != 1.0 {
                log::trace!(
                    "physically scaling {:?} by {} bcos {}x{} > {:?}x{:?}. aspect={}",
                    info,
                    scale,
                    glyph.width,
                    glyph.height,
                    cell_width,
                    cell_height,
                    aspect,
                );
                (1.0, raw_im.scale_by(scale))
            } else {
                (scale, raw_im)
            };

            let tex = self.atlas.allocate(&raw_im)?;

            let g = CachedGlyph {
                has_color: glyph.has_color,
                texture: Some(tex),
                x_offset,
                y_offset,
                bearing_x,
                bearing_y,
                scale,
            };

            if info.font_idx != 0 {
                // It's generally interesting to examine eg: emoji or ligatures
                // that we might have fallen back to
                log::trace!("{:?} {:?}", info, g);
            }

            g
        };

        Ok(Rc::new(glyph))
    }

    pub fn cached_image(
        &mut self,
        image_data: &Arc<ImageData>,
        padding: Option<usize>,
    ) -> anyhow::Result<(Sprite<T>, Option<Instant>)> {
        let id = image_data.id();
        if let Some(cached) = self.image_cache.get_mut(&id) {
            match cached {
                CachedImage::SingleFrame => {
                    // We can simply use the frame cache to manage
                    // the texture space; the frame is always 0 for
                    // a single frame
                    if let Some(sprite) = self.frame_cache.get(&(id, 0)) {
                        return Ok((sprite.clone(), None));
                    }
                }
                CachedImage::Animation(decoded) => {
                    let mut next = None;
                    if decoded.frames.len() > 1 {
                        let now = Instant::now();
                        let mut next_due =
                            decoded.frame_start + decoded.frames[decoded.current_frame].duration;
                        if now >= next_due {
                            // Advance to next frame
                            decoded.current_frame += 1;
                            if decoded.current_frame >= decoded.frames.len() {
                                decoded.current_frame = 0;
                            }
                            decoded.frame_start = now;
                            next_due = decoded.frame_start
                                + decoded.frames[decoded.current_frame].duration;
                        }

                        next.replace(next_due);
                    }

                    if let Some(sprite) = self.frame_cache.get(&(id, decoded.current_frame)) {
                        return Ok((sprite.clone(), next));
                    }

                    let sprite = self.atlas.allocate_with_padding(
                        &decoded.frames[decoded.current_frame].image,
                        padding,
                    )?;

                    self.frame_cache
                        .insert((id, decoded.current_frame), sprite.clone());

                    return Ok((
                        sprite,
                        Some(decoded.frame_start + decoded.frames[decoded.current_frame].duration),
                    ));
                }
            }
        }

        let decoded =
            DecodedImage::load(image_data).or_else(|e| -> anyhow::Result<DecodedImage> {
                log::debug!("Failed to decode image: {:#}", e);
                // Use a placeholder instead
                Ok(DecodedImage::placeholder())
            })?;
        let sprite = self
            .atlas
            .allocate_with_padding(&decoded.frames[0].image, padding)?;
        self.frame_cache.insert((id, 0), sprite.clone());
        if decoded.frames.len() > 1 {
            let next = Some(decoded.frame_start + decoded.frames[0].duration);
            self.image_cache.put(id, CachedImage::Animation(decoded));
            Ok((sprite, next))
        } else {
            self.image_cache.put(id, CachedImage::SingleFrame);
            Ok((sprite, None))
        }
    }

    fn block_sprite(&mut self, block: BlockKey) -> anyhow::Result<Sprite<T>> {
        let mut buffer = Image::new(
            self.metrics.cell_size.width as usize,
            self.metrics.cell_size.height as usize,
        );
        let black = SrgbaPixel::rgba(0, 0, 0, 0);
        let white = SrgbaPixel::rgba(0xff, 0xff, 0xff, 0xff);

        let cell_rect = Rect::new(Point::new(0, 0), self.metrics.cell_size);

        let y_eighth = self.metrics.cell_size.height as f32 / 8.;
        let x_eighth = self.metrics.cell_size.width as f32 / 8.;

        fn scale(f: f32) -> usize {
            f.ceil().max(1.) as usize
        }

        buffer.clear_rect(cell_rect, black);

        let draw_horizontal = |buffer: &mut Image, y: usize| {
            buffer.draw_line(
                Point::new(cell_rect.origin.x, cell_rect.origin.y + y as isize),
                Point::new(
                    cell_rect.origin.x + self.metrics.cell_size.width,
                    cell_rect.origin.y + y as isize,
                ),
                white,
            );
        };

        let draw_vertical = |buffer: &mut Image, x: usize| {
            buffer.draw_line(
                Point::new(cell_rect.origin.x + x as isize, cell_rect.origin.y),
                Point::new(
                    cell_rect.origin.x + x as isize,
                    cell_rect.origin.y + self.metrics.cell_size.height,
                ),
                white,
            );
        };

        let draw_quad = |buffer: &mut Image, x: Range<usize>, y: Range<usize>| {
            for y in y {
                buffer.draw_line(
                    Point::new(
                        cell_rect.origin.x + x.start as isize,
                        cell_rect.origin.y + y as isize,
                    ),
                    Point::new(
                        // Note: draw_line uses inclusive coordinates, but our
                        // range is exclusive coordinates, so compensate here!
                        // We don't need to do this for `y` since we are already
                        // iterating over the correct set of `y` values in our loop.
                        cell_rect.origin.x + x.end.saturating_sub(1) as isize,
                        cell_rect.origin.y + y as isize,
                    ),
                    white,
                );
            }
        };

        match block {
            BlockKey::Upper(num) => {
                for n in 0..usize::from(num) {
                    for a in 0..scale(y_eighth) {
                        draw_horizontal(&mut buffer, (n as f32 * y_eighth).floor() as usize + a);
                    }
                }
            }
            BlockKey::Lower(num) => {
                for n in 0..usize::from(num) {
                    let y =
                        (self.metrics.cell_size.height - 1) as usize - scale(n as f32 * y_eighth);
                    for a in 0..scale(y_eighth) {
                        draw_horizontal(&mut buffer, y + a);
                    }
                }
            }
            BlockKey::Left(num) => {
                for n in 0..usize::from(num) {
                    for a in 0..scale(x_eighth) {
                        draw_vertical(&mut buffer, (n as f32 * x_eighth).floor() as usize + a);
                    }
                }
            }
            BlockKey::Right(num) => {
                for n in 0..usize::from(num) {
                    let x =
                        (self.metrics.cell_size.width - 1) as usize - scale(n as f32 * x_eighth);
                    for a in 0..scale(x_eighth) {
                        draw_vertical(&mut buffer, x + a);
                    }
                }
            }
            BlockKey::Full(alpha) => {
                let alpha = match alpha {
                    BlockAlpha::Full => 1.0,
                    BlockAlpha::Dark => 0.75,
                    BlockAlpha::Medium => 0.5,
                    BlockAlpha::Light => 0.25,
                };
                let fill = LinearRgba::with_components(alpha, alpha, alpha, alpha);

                buffer.clear_rect(cell_rect, fill.srgba_pixel());
            }
            BlockKey::Quadrants(quads) => {
                let y_half = self.metrics.cell_size.height as f32 / 2.;
                let x_half = self.metrics.cell_size.width as f32 / 2.;
                let width = self.metrics.cell_size.width as usize;
                let height = self.metrics.cell_size.height as usize;
                if quads.contains(Quadrant::UPPER_LEFT) {
                    draw_quad(&mut buffer, 0..scale(x_half), 0..scale(y_half));
                }
                if quads.contains(Quadrant::UPPER_RIGHT) {
                    draw_quad(&mut buffer, scale(x_half)..width, 0..scale(y_half));
                }
                if quads.contains(Quadrant::LOWER_LEFT) {
                    draw_quad(&mut buffer, 0..scale(x_half), scale(y_half)..height);
                }
                if quads.contains(Quadrant::LOWER_RIGHT) {
                    draw_quad(&mut buffer, scale(x_half)..width, scale(y_half)..height);
                }
            }
        }

        /*
        log::info!("{:?}", block);
        buffer.log_bits();
        */

        self.atlas.allocate(&buffer).map_err(Into::into)
    }

    pub fn cached_custom_glyph(
        &mut self,
        custom_glyph: CustomGlyphKey,
    ) -> anyhow::Result<Sprite<T>> {
        if let Some(s) = self.custom_glyphs.get(&custom_glyph) {
            return Ok(s.clone());
        }

        let sprite = match custom_glyph {
            CustomGlyphKey::Block(block) => self.block_sprite(block)?,
            CustomGlyphKey::BoxDrawing(box_drawing) => self.box_drawing_sprite(box_drawing)?,
        };

        self.custom_glyphs.insert(custom_glyph, sprite.clone());
        Ok(sprite)
    }

    fn box_drawing_sprite(&mut self, box_drawing: BoxDrawingKey) -> anyhow::Result<Sprite<T>> {
        let mut buffer = Image::new(
            self.metrics.cell_size.width as usize,
            self.metrics.cell_size.height as usize,
        );
        let black = SrgbaPixel::rgba(0, 0, 0, 0);
        let white = SrgbaPixel::rgba(0xff, 0xff, 0xff, 0xff);

        let cell_rect = Rect::new(Point::new(0, 0), self.metrics.cell_size);

        buffer.clear_rect(cell_rect, black);

        let draw_rect = |buffer: &mut Image, rect: Box2D<usize, PixelUnit>| {
            for x in rect.min.x..rect.max.x {
                for y in rect.min.y..rect.max.y {
                    let pixel = buffer.pixel_mut(x, y);
                    *pixel = white.as_srgba32();
                }
            }
        };

        let center = cell_rect.center();
        let light_thickness = self.metrics.underline_height as usize;
        let heavy_thickness = light_thickness * 2;

        use BoxDrawingKey::*;
        match box_drawing {
            LightHorizontal => {
                let half_thickness = (light_thickness / 2) as isize;

                let min = Point::new(cell_rect.min_x(), center.y - half_thickness).to_usize();
                let max = Point::new(cell_rect.max_x(), center.y + half_thickness).to_usize();

                draw_rect(&mut buffer, Box2D::new(min, max))
            }
            HeavyHorizontal => {
                let half_thickness = (heavy_thickness / 2) as isize;

                let min = Point::new(cell_rect.min_x(), center.y - half_thickness).to_usize();
                let max = Point::new(cell_rect.max_x(), center.y + half_thickness).to_usize();

                draw_rect(&mut buffer, Box2D::new(min, max))
            }
            LightVertical => {
                let half_thickness = (light_thickness / 2) as isize;

                let min = Point::new(center.x - half_thickness, cell_rect.min_y()).to_usize();
                let max = Point::new(center.x + half_thickness, cell_rect.max_y()).to_usize();

                draw_rect(&mut buffer, Box2D::new(min, max))
            }
            HeavyVertical => {
                let half_thickness = (heavy_thickness / 2) as isize;

                let min = Point::new(center.x - half_thickness, cell_rect.min_y()).to_usize();
                let max = Point::new(center.x + half_thickness, cell_rect.max_y()).to_usize();

                draw_rect(&mut buffer, Box2D::new(min, max))
            }
        }

        self.atlas.allocate(&buffer).map_err(Into::into)
    }

    fn line_sprite(&mut self, key: LineKey) -> anyhow::Result<Sprite<T>> {
        let mut buffer = Image::new(
            self.metrics.cell_size.width as usize,
            self.metrics.cell_size.height as usize,
        );
        let black = SrgbaPixel::rgba(0, 0, 0, 0);
        let white = SrgbaPixel::rgba(0xff, 0xff, 0xff, 0xff);

        let cell_rect = Rect::new(Point::new(0, 0), self.metrics.cell_size);

        let draw_single = |buffer: &mut Image| {
            for row in 0..self.metrics.underline_height {
                buffer.draw_line(
                    Point::new(
                        cell_rect.origin.x,
                        cell_rect.origin.y + self.metrics.descender_row + row,
                    ),
                    Point::new(
                        cell_rect.origin.x + self.metrics.cell_size.width,
                        cell_rect.origin.y + self.metrics.descender_row + row,
                    ),
                    white,
                );
            }
        };

        let draw_dotted = |buffer: &mut Image| {
            for row in 0..self.metrics.underline_height {
                let y = (cell_rect.origin.y + self.metrics.descender_row + row) as usize;
                if y >= self.metrics.cell_size.height as usize {
                    break;
                }

                let mut color = white;
                let segment_length = (self.metrics.cell_size.width / 4) as usize;
                let mut count = segment_length;
                let range =
                    buffer.horizontal_pixel_range_mut(0, self.metrics.cell_size.width as usize, y);
                for c in range.iter_mut() {
                    *c = color.as_srgba32();
                    count -= 1;
                    if count == 0 {
                        color = if color == white { black } else { white };
                        count = segment_length;
                    }
                }
            }
        };

        let draw_dashed = |buffer: &mut Image| {
            for row in 0..self.metrics.underline_height {
                let y = (cell_rect.origin.y + self.metrics.descender_row + row) as usize;
                if y >= self.metrics.cell_size.height as usize {
                    break;
                }
                let mut color = white;
                let third = (self.metrics.cell_size.width / 3) as usize + 1;
                let mut count = third;
                let range =
                    buffer.horizontal_pixel_range_mut(0, self.metrics.cell_size.width as usize, y);
                for c in range.iter_mut() {
                    *c = color.as_srgba32();
                    count -= 1;
                    if count == 0 {
                        color = if color == white { black } else { white };
                        count = third;
                    }
                }
            }
        };

        let draw_curly = |buffer: &mut Image| {
            let max_y = self.metrics.cell_size.height as usize - 1;
            let x_factor = (2. * std::f32::consts::PI) / self.metrics.cell_size.width as f32;

            // Have the wave go from the descender to the bottom of the cell
            let wave_height =
                self.metrics.cell_size.height - (cell_rect.origin.y + self.metrics.descender_row);

            let half_height = (wave_height as f32 / 2.).max(1.);
            let y =
                (cell_rect.origin.y + self.metrics.descender_row) as usize - half_height as usize;

            fn add(x: usize, y: usize, val: u8, max_y: usize, buffer: &mut Image) {
                let y = y.min(max_y);
                let pixel = buffer.pixel_mut(x, y);
                let (current, _, _, _) = SrgbaPixel::with_srgba_u32(*pixel).as_rgba();
                let value = current.saturating_add(val);
                *pixel = SrgbaPixel::rgba(value, value, value, 0xff).as_srgba32();
            }

            for x in 0..self.metrics.cell_size.width as usize {
                let vertical = wave_height as f32 * (x as f32 * x_factor).cos();
                let v1 = vertical.floor();
                let v2 = vertical.ceil();

                for row in 0..self.metrics.underline_height as usize {
                    let value = (255. * (vertical - v1).abs()) as u8;
                    add(x, row + y + v1 as usize, 255 - value, max_y, buffer);
                    add(x, row + y + v2 as usize, value, max_y, buffer);
                }
            }
        };

        let draw_double = |buffer: &mut Image| {
            let first_line = self
                .metrics
                .descender_row
                .min(self.metrics.descender_plus_two - 2 * self.metrics.underline_height);

            for row in 0..self.metrics.underline_height {
                buffer.draw_line(
                    Point::new(cell_rect.origin.x, cell_rect.origin.y + first_line + row),
                    Point::new(
                        cell_rect.origin.x + self.metrics.cell_size.width,
                        cell_rect.origin.y + first_line + row,
                    ),
                    white,
                );
                buffer.draw_line(
                    Point::new(
                        cell_rect.origin.x,
                        cell_rect.origin.y + self.metrics.descender_plus_two + row,
                    ),
                    Point::new(
                        cell_rect.origin.x + self.metrics.cell_size.width,
                        cell_rect.origin.y + self.metrics.descender_plus_two + row,
                    ),
                    white,
                );
            }
        };

        let draw_strike = |buffer: &mut Image| {
            for row in 0..self.metrics.underline_height {
                buffer.draw_line(
                    Point::new(
                        cell_rect.origin.x,
                        cell_rect.origin.y + self.metrics.strike_row + row,
                    ),
                    Point::new(
                        cell_rect.origin.x + self.metrics.cell_size.width,
                        cell_rect.origin.y + self.metrics.strike_row + row,
                    ),
                    white,
                );
            }
        };

        let draw_overline = |buffer: &mut Image| {
            for row in 0..self.metrics.underline_height {
                buffer.draw_line(
                    Point::new(cell_rect.origin.x, cell_rect.origin.y + row),
                    Point::new(
                        cell_rect.origin.x + self.metrics.cell_size.width,
                        cell_rect.origin.y + row,
                    ),
                    white,
                );
            }
        };

        buffer.clear_rect(cell_rect, black);
        if key.overline {
            draw_overline(&mut buffer);
        }
        match key.underline {
            Underline::None => {}
            Underline::Single => draw_single(&mut buffer),
            Underline::Curly => draw_curly(&mut buffer),
            Underline::Dashed => draw_dashed(&mut buffer),
            Underline::Dotted => draw_dotted(&mut buffer),
            Underline::Double => draw_double(&mut buffer),
        }
        if key.strike_through {
            draw_strike(&mut buffer);
        }
        let sprite = self.atlas.allocate(&buffer)?;
        self.line_glyphs.insert(key, sprite.clone());
        Ok(sprite)
    }

    /// Figure out what we're going to draw for the underline.
    /// If the current cell is part of the current URL highlight
    /// then we want to show the underline.
    pub fn cached_line_sprite(
        &mut self,
        is_highlited_hyperlink: bool,
        is_strike_through: bool,
        underline: Underline,
        overline: bool,
    ) -> anyhow::Result<Sprite<T>> {
        let effective_underline = match (is_highlited_hyperlink, underline) {
            (true, Underline::None) => Underline::Single,
            (true, Underline::Single) => Underline::Double,
            (true, _) => Underline::Single,
            (false, u) => u,
        };

        let key = LineKey {
            strike_through: is_strike_through,
            overline,
            underline: effective_underline,
        };

        if let Some(s) = self.line_glyphs.get(&key) {
            return Ok(s.clone());
        }

        self.line_sprite(key)
    }
}
