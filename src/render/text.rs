//! Text: fonts found with fontdb, shaped with harfrust (HarfBuzz's
//! algorithms), drawn from their outlines (skrifa) through tiny-skia, and
//! kept in a cache of rendered glyphs.
//!
//! Only the fonts actually used are opened (memory-mapped, so they cost
//! page cache rather than heap). Characters a font lacks are looked up in
//! fallback fonts, then any installed font, once each.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use harfrust::{Buffer, Direction, ShapeOptions, ShaperFont};
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::{FontRef, GlyphId, MetadataProvider};
use tiny_skia::{FillRule, Mask, PathBuilder, PixmapMut, Transform};
use unicode_segmentation::UnicodeSegmentation;

use crate::geom::{IRect, Point};
use crate::render::theme::{Color, FONT_FAMILIES, MONO_FAMILIES};

pub const NORMAL: u16 = 400;
pub const MEDIUM: u16 = 500;
pub const DEMIBOLD: u16 = 600;
pub const BOLD: u16 = 700;

/// A font as the UI asks for one: a size in logical pixels, a weight, and
/// whether it's the monospaced one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FontSpec {
    pub px: f32,
    pub weight: u16,
    pub mono: bool,
}

/// Flatshot's sans font at `px` logical pixels.
pub fn font(px: f32, weight: u16) -> FontSpec {
    FontSpec { px, weight, mono: false }
}

pub fn mono(px: f32, weight: u16) -> FontSpec {
    FontSpec { px, weight, mono: true }
}

impl FontSpec {
    fn key(&self) -> (u32, u16, bool) {
        ((self.px * 64.0).round() as u32, self.weight, self.mono)
    }
}

/// QFontMetricsF's numbers for a font.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    pub ascent: f32,
    pub descent: f32,
    pub leading: f32,
    pub height: f32,
    pub line_spacing: f32,
    pub avg_char_width: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct Placed {
    pub face: u32,
    pub id: u32,
    pub weight: u16,
    /// size in logical pixels
    pub px: f32,
    /// from the line's start, baseline at y = 0
    pub x: f32,
    pub y: f32,
}

/// One shaped line of text.
#[derive(Clone, Debug, Default)]
pub struct Line {
    pub glyphs: Vec<Placed>,
    pub width: f32,
}

// -- the font system ------------------------------------------------------------

type FontData = Arc<dyn AsRef<[u8]> + Send + Sync>;

struct Face {
    data: FontData,
    index: u32,
    /// The weight axis (min, max) of a variable font.
    wght: Option<(f32, f32)>,
    upem: f32,
    shapers: HashMap<u16, harfrust::font::Font>,
}

impl Face {
    fn font_ref(&self) -> Option<FontRef<'_>> {
        FontRef::from_index((*self.data).as_ref(), self.index).ok()
    }

    fn covers(&self, c: char) -> bool {
        self.font_ref().is_some_and(|f| f.charmap().map(c).is_some())
    }

    /// The weight to apply to a variable font, if it has the axis.
    fn wght_for(&self, weight: u16) -> Option<f32> {
        self.wght.map(|(lo, hi)| (weight as f32).clamp(lo, hi))
    }

    fn shaper_font(&mut self, weight: u16) -> Option<&harfrust::font::Font> {
        let w = if self.wght.is_some() { weight } else { 0 };
        if !self.shapers.contains_key(&w) {
            let blob = harfrust::font::Blob::Shared(self.data.clone());
            let base = harfrust::font::Font::new(blob, self.index)?;
            let font = match self.wght_for(weight) {
                Some(v) => base.instance_builder().variations([("wght", v)]).build(),
                None => base,
            };
            self.shapers.insert(w, font);
        }
        self.shapers.get(&w)
    }
}

/// A rendered glyph: an alpha mask, or a colour picture (emoji).
pub struct Glyph {
    pub left: i32,
    pub top: i32,
    pub w: u32,
    pub h: u32,
    pub alpha: Vec<u8>,
    /// premultiplied BGRA when the glyph has its own colours
    pub color: Option<Vec<u8>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey {
    face: u32,
    id: u32,
    weight: u16,
    /// device pixels * 64
    size: u32,
    /// the horizontal position's fraction, in quarters of a pixel
    frac: u8,
}

struct System {
    db: fontdb::Database,
    faces: Vec<Face>,
    by_id: HashMap<fontdb::ID, u32>,
    primary: HashMap<(bool, u16), u32>,
    fallback: HashMap<(char, bool), Option<u32>>,
    glyphs: HashMap<GlyphKey, Option<Arc<Glyph>>>,
    glyph_bytes: usize,
    lines: HashMap<(String, (u32, u16, bool)), Arc<Line>>,
    metrics: HashMap<(u32, u16, bool), Metrics>,
}

/// Cached glyph pictures beyond this many bytes are dropped.
const GLYPH_CACHE_BYTES: usize = 4 << 20;
const LINE_CACHE: usize = 512;

/// Families tried for characters the UI font lacks, before any font at all.
const FALLBACK_FAMILIES: &[&str] = &[
    "Noto Sans",
    "DejaVu Sans",
    "Noto Sans CJK SC",
    "Noto Sans CJK JP",
    "Noto Sans CJK KR",
    "Noto Sans CJK TC",
    "Source Han Sans SC",
    "WenQuanYi Zen Hei",
    "Droid Sans Fallback",
    "Noto Sans Arabic",
    "Noto Naskh Arabic",
    "Noto Sans Hebrew",
    "Noto Sans Devanagari",
    "Noto Sans Thai",
    "Noto Sans Symbols",
    "Noto Sans Symbols2",
    "Noto Sans Math",
    "Liberation Sans",
    "FreeSans",
    "Unifont",
];
const EMOJI_FAMILIES: &[&str] = &["Noto Color Emoji", "Twemoji", "JoyPixels", "Apple Color Emoji", "Noto Emoji", "Symbola"];

static SYSTEM: OnceLock<Mutex<System>> = OnceLock::new();

fn sys() -> MutexGuard<'static, System> {
    SYSTEM
        .get_or_init(|| {
            let mut db = fontdb::Database::new();
            db.load_system_fonts();
            // A font Flatshot can always find when the system has none of
            // the families it asks for (or none at all): DejaVu or the like.
            Mutex::new(System {
                db,
                faces: Vec::new(),
                by_id: HashMap::new(),
                primary: HashMap::new(),
                fallback: HashMap::new(),
                glyphs: HashMap::new(),
                glyph_bytes: 0,
                lines: HashMap::new(),
                metrics: HashMap::new(),
            })
        })
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Find the fonts and shape the strings the overlay shows first, so the
/// first capture doesn't wait for them (the tray app calls this at start).
pub fn warm_up() {
    for w in [MEDIUM, DEMIBOLD, BOLD] {
        let _ = shape("Drag to capture · 0123456789 × #ABCDEF", font(12.0, w));
    }
    let _ = shape("300, 600   #F5F3EF", mono(12.0, MEDIUM));
}

/// How many fonts are known and how many are open (for --diagnose).
pub fn stats() -> (usize, usize) {
    let s = sys();
    (s.db.len(), s.faces.len())
}

/// The family of the UI font in use (for --diagnose).
pub fn primary_family(mono: bool) -> String {
    let mut s = sys();
    let Some(face) = s.primary_face(mono, MEDIUM) else { return "none".into() };
    let id = s.by_id.iter().find(|(_, v)| **v == face).map(|(k, _)| *k);
    id.and_then(|id| s.db.face(id).map(|f| f.families.first().map(|f| f.0.clone()).unwrap_or_default()))
        .unwrap_or_default()
}

impl System {
    fn open(&mut self, id: fontdb::ID) -> Option<u32> {
        if let Some(&i) = self.by_id.get(&id) {
            return Some(i);
        }
        // SAFETY: the font file is mapped read-only. If something truncated
        // it while mapped, reading it could fault; fonts are installed
        // files that package managers replace (new inodes), never shrink.
        let (data, index) = unsafe { self.db.make_shared_face_data(id) }?;
        let font = FontRef::from_index((*data).as_ref(), index).ok()?;
        let upem = font.metrics(Size::unscaled(), LocationRef::default()).units_per_em.max(1) as f32;
        let wght = font.axes().iter().find(|a| a.tag() == skrifa::Tag::new(b"wght")).map(|a| (a.min_value(), a.max_value()));
        let i = self.faces.len() as u32;
        self.faces.push(Face { data, index, wght, upem, shapers: HashMap::new() });
        self.by_id.insert(id, i);
        Some(i)
    }

    fn query(&mut self, families: &[&str], weight: u16) -> Option<u32> {
        for name in families {
            let fam = [fontdb::Family::Name(name)];
            let q = fontdb::Query { families: &fam, weight: fontdb::Weight(weight), ..Default::default() };
            if let Some(id) = self.db.query(&q)
                && let Some(face) = self.open(id)
            {
                return Some(face);
            }
        }
        None
    }

    fn primary_face(&mut self, mono: bool, weight: u16) -> Option<u32> {
        if let Some(&f) = self.primary.get(&(mono, weight)) {
            return Some(f);
        }
        let families = if mono { MONO_FAMILIES } else { FONT_FAMILIES };
        let mut found = self.query(families, weight);
        if found.is_none() {
            let generic = [if mono { fontdb::Family::Monospace } else { fontdb::Family::SansSerif }];
            let q = fontdb::Query { families: &generic, weight: fontdb::Weight(weight), ..Default::default() };
            found = self.db.query(&q).and_then(|id| self.open(id));
        }
        if found.is_none() {
            // Anything upright at all.
            let id = self.db.faces().find(|f| f.style == fontdb::Style::Normal).map(|f| f.id);
            found = id.and_then(|id| self.open(id));
        }
        let f = found?;
        self.primary.insert((mono, weight), f);
        Some(f)
    }

    /// The face to draw `c` with when `base` lacks it.
    fn face_for(&mut self, c: char, base: u32, mono: bool, weight: u16) -> u32 {
        if is_invisible(c) || self.faces[base as usize].covers(c) {
            return base;
        }
        if let Some(found) = self.fallback.get(&(c, mono)) {
            return found.unwrap_or(base);
        }
        let mut found = None;
        let lists: [&[&str]; 2] = if is_emoji(c) { [EMOJI_FAMILIES, FALLBACK_FAMILIES] } else { [FALLBACK_FAMILIES, EMOJI_FAMILIES] };
        'lists: for list in lists {
            for name in list {
                if let Some(f) = self.query(&[name], weight)
                    && self.faces[f as usize].covers(c)
                {
                    found = Some(f);
                    break 'lists;
                }
            }
        }
        if found.is_none() {
            // Any installed font, upright ones first; each tried once.
            let mut ids: Vec<(bool, fontdb::ID)> = self.db.faces().map(|f| (f.style != fontdb::Style::Normal, f.id)).collect();
            ids.sort_by_key(|(italic, _)| *italic);
            for (_, id) in ids.into_iter().take(1500) {
                if let Some(f) = self.open(id)
                    && self.faces[f as usize].covers(c)
                {
                    found = Some(f);
                    break;
                }
            }
        }
        self.fallback.insert((c, mono), found);
        found.unwrap_or(base)
    }

    fn metrics(&mut self, spec: FontSpec) -> Metrics {
        if let Some(m) = self.metrics.get(&spec.key()) {
            return *m;
        }
        let fallback = Metrics {
            ascent: spec.px * 0.95,
            descent: spec.px * 0.25,
            leading: 0.0,
            height: spec.px * 1.2,
            line_spacing: spec.px * 1.2,
            avg_char_width: spec.px * 0.55,
        };
        let Some(f) = self.primary_face(spec.mono, spec.weight) else { return fallback };
        let face = &self.faces[f as usize];
        let Some(font) = face.font_ref() else { return fallback };
        let loc = face.wght_for(spec.weight).map(|v| font.axes().location([("wght", v)]));
        let m = match &loc {
            Some(l) => font.metrics(Size::new(spec.px), l),
            None => font.metrics(Size::new(spec.px), LocationRef::default()),
        };
        let ascent = m.ascent;
        let descent = -m.descent;
        let leading = m.leading.max(0.0);
        let out = Metrics {
            ascent,
            descent,
            leading,
            height: ascent + descent,
            line_spacing: ascent + descent + leading,
            avg_char_width: m.average_width.unwrap_or(spec.px * 0.5),
        };
        self.metrics.insert(spec.key(), out);
        out
    }

    fn shape(&mut self, text: &str, spec: FontSpec) -> Arc<Line> {
        let key = (text.to_string(), spec.key());
        if let Some(l) = self.lines.get(&key) {
            return l.clone();
        }
        let line = Arc::new(self.shape_uncached(text, spec));
        if self.lines.len() >= LINE_CACHE {
            self.lines.clear();
        }
        self.lines.insert(key, line.clone());
        line
    }

    fn shape_uncached(&mut self, text: &str, spec: FontSpec) -> Line {
        let mut line = Line::default();
        let Some(base) = self.primary_face(spec.mono, spec.weight) else { return line };
        if text.is_empty() {
            return line;
        }
        // Runs in visual order, each one direction.
        let runs: Vec<(std::ops::Range<usize>, bool)> = if text.chars().any(is_rtl_char) {
            let bidi = unicode_bidi::BidiInfo::new(text, None);
            let mut out = Vec::new();
            for para in &bidi.paragraphs {
                let (levels, ranges) = bidi.visual_runs(para, para.range.clone());
                for r in ranges {
                    let rtl = levels[r.start].is_rtl();
                    out.push((r, rtl));
                }
            }
            out
        } else {
            vec![(0..text.len(), false)]
        };
        let mut x = 0.0f32;
        for (range, rtl) in runs {
            // Split the run where the font changes (by grapheme, so marks
            // stay with their letter).
            let mut pieces: Vec<(std::ops::Range<usize>, u32)> = Vec::new();
            for (off, g) in text[range.clone()].grapheme_indices(true) {
                let c = g.chars().next().unwrap_or(' ');
                let face = self.face_for(c, base, spec.mono, spec.weight);
                let start = range.start + off;
                match pieces.last_mut() {
                    Some((r, f)) if *f == face => r.end = start + g.len(),
                    _ => pieces.push((start..start + g.len(), face)),
                }
            }
            if rtl {
                pieces.reverse();
            }
            for (r, face) in pieces {
                x = self.shape_piece(&text[r.clone()], r.start, face, rtl, spec, x, &mut line);
            }
        }
        line.width = x;
        line
    }

    #[allow(clippy::too_many_arguments)]
    fn shape_piece(&mut self, s: &str, _start: usize, face: u32, rtl: bool, spec: FontSpec, x0: f32, line: &mut Line) -> f32 {
        let f = &mut self.faces[face as usize];
        let upem = f.upem;
        let Some(font) = f.shaper_font(spec.weight) else { return x0 };
        let shaper = ShaperFont::new(font);
        let mut buf = Buffer::new();
        buf.push_str(s);
        buf.set_direction(if rtl { Direction::RightToLeft } else { Direction::LeftToRight });
        buf.guess_segment_properties();
        if harfrust::shape(&shaper, &mut buf, ShapeOptions::new()).is_err() {
            return x0;
        }
        let k = spec.px / upem;
        let mut x = x0;
        for (info, pos) in buf.glyph_infos().iter().zip(buf.glyph_positions()) {
            line.glyphs.push(Placed {
                face,
                id: info.glyph_id,
                weight: spec.weight,
                px: spec.px,
                x: x + pos.x_offset as f32 * k,
                y: -pos.y_offset as f32 * k,
            });
            x += pos.x_advance as f32 * k;
        }
        x
    }

    fn glyph(&mut self, key: GlyphKey) -> Option<Arc<Glyph>> {
        if let Some(g) = self.glyphs.get(&key) {
            return g.clone();
        }
        let g = self.render_glyph(key).map(Arc::new);
        if let Some(g) = &g {
            self.glyph_bytes += g.alpha.len() + g.color.as_ref().map_or(0, |c| c.len()) + 64;
        }
        if self.glyph_bytes > GLYPH_CACHE_BYTES {
            self.glyphs.clear();
            self.glyph_bytes = 0;
        }
        self.glyphs.insert(key, g.clone());
        g
    }

    fn render_glyph(&self, key: GlyphKey) -> Option<Glyph> {
        let face = &self.faces[key.face as usize];
        let font = face.font_ref()?;
        let size = key.size as f32 / 64.0;
        let gid = GlyphId::new(key.id);
        let loc = face.wght_for(key.weight).map(|v| font.axes().location([("wght", v)]));
        let loc_ref: LocationRef<'_> = loc.as_ref().map(LocationRef::from).unwrap_or_default();
        let outlines = font.outline_glyphs();
        if let Some(outline) = outlines.get(gid) {
            let mut pen = Pen { pb: PathBuilder::new(), dx: key.frac as f32 / 4.0 };
            if outline.draw(DrawSettings::unhinted(Size::new(size), loc_ref), &mut pen).is_ok() {
                let Some(path) = pen.pb.finish() else {
                    return Some(Glyph { left: 0, top: 0, w: 0, h: 0, alpha: Vec::new(), color: None });
                };
                let b = path.bounds();
                let left = b.left().floor() as i32 - 1;
                let top = b.top().floor() as i32 - 1;
                let w = (b.right().ceil() as i32 + 1 - left).max(1) as u32;
                let h = (b.bottom().ceil() as i32 + 1 - top).max(1) as u32;
                if w > 2048 || h > 2048 {
                    return None;
                }
                let mut mask = Mask::new(w, h)?;
                mask.fill_path(&path, FillRule::Winding, true, Transform::from_translate(-left as f32, -top as f32));
                return Some(Glyph { left, top, w, h, alpha: mask.data().to_vec(), color: None });
            }
        }
        // No outline: a colour bitmap (emoji fonts like Noto Color Emoji).
        let strikes = font.bitmap_strikes();
        let bitmap = strikes.glyph_for_size(Size::new(size), gid)?;
        let (bw, bh) = (bitmap.width, bitmap.height);
        let bgra = match bitmap.data {
            skrifa::bitmap::BitmapData::Png(bytes) => decode_png_bgra(bytes)?.2,
            skrifa::bitmap::BitmapData::Bgra(bytes) => bytes.to_vec(),
            skrifa::bitmap::BitmapData::Mask(_) => return None,
        };
        let (bw, bh) = if bgra.len() == (bw * bh * 4) as usize { (bw, bh) } else { return None };
        let scale = size / bitmap.ppem_y.max(1.0);
        let w = ((bw as f32 * scale).round() as u32).max(1);
        let h = ((bh as f32 * scale).round() as u32).max(1);
        let src = crate::image::Image::from_bgra(bw, bh, bgra).ok()?;
        let scaled = if (w, h) == (bw, bh) { src } else { src.scaled_area(w, h).ok()? };
        let left = (bitmap.inner_bearing_x * scale).round() as i32 + key.frac as i32 / 4;
        let top = match bitmap.placement_origin {
            skrifa::bitmap::Origin::TopLeft => -(bitmap.inner_bearing_y * scale).round() as i32,
            skrifa::bitmap::Origin::BottomLeft => -((bitmap.inner_bearing_y * scale).round() as i32) - h as i32,
        };
        let data = scaled.data().to_vec();
        let alpha = data.chunks_exact(4).map(|p| p[3]).collect();
        Some(Glyph { left, top, w, h, alpha, color: Some(data) })
    }
}

struct Pen {
    pb: PathBuilder,
    dx: f32,
}

impl OutlinePen for Pen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.pb.move_to(x + self.dx, -y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.pb.line_to(x + self.dx, -y);
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        self.pb.quad_to(cx + self.dx, -cy, x + self.dx, -y);
    }
    fn curve_to(&mut self, c0x: f32, c0y: f32, c1x: f32, c1y: f32, x: f32, y: f32) {
        self.pb.cubic_to(c0x + self.dx, -c0y, c1x + self.dx, -c1y, x + self.dx, -y);
    }
    fn close(&mut self) {
        self.pb.close();
    }
}

/// Outlines into a path at an offset (for text drawn as a shape).
struct OffsetPen<'a> {
    pb: &'a mut PathBuilder,
    x: f32,
    y: f32,
}

impl OutlinePen for OffsetPen<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.pb.move_to(self.x + x, self.y - y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.pb.line_to(self.x + x, self.y - y);
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        self.pb.quad_to(self.x + cx, self.y - cy, self.x + x, self.y - y);
    }
    fn curve_to(&mut self, c0x: f32, c0y: f32, c1x: f32, c1y: f32, x: f32, y: f32) {
        self.pb.cubic_to(self.x + c0x, self.y - c0y, self.x + c1x, self.y - c1y, self.x + x, self.y - y);
    }
    fn close(&mut self) {
        self.pb.close();
    }
}

fn decode_png_bgra(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let img = crate::codec::decode_png(bytes).ok()?;
    let (w, h) = img.size();
    Some((w, h, img.data().to_vec()))
}

fn is_rtl_char(c: char) -> bool {
    matches!(c as u32, 0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF | 0x10800..=0x10FFF | 0x1E800..=0x1EFFF)
}

fn is_emoji(c: char) -> bool {
    matches!(c as u32, 0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF | 0x1FC00..=0x1FFFF)
}

/// Characters drawn as nothing (joiners, selectors): never worth a fallback.
fn is_invisible(c: char) -> bool {
    matches!(c as u32, 0x200B..=0x200F | 0x2060..=0x206F | 0xFE00..=0xFE0F | 0xE0100..=0xE01EF | 0xFEFF) || c.is_control()
}

// -- the public API ---------------------------------------------------------------

/// Shape one line of text (no line breaks in it).
pub fn shape(text: &str, spec: FontSpec) -> Arc<Line> {
    sys().shape(text, spec)
}

/// The advance of `text` (QFontMetricsF::horizontalAdvance).
pub fn width(text: &str, spec: FontSpec) -> f32 {
    shape(text, spec).width
}

pub fn metrics(spec: FontSpec) -> Metrics {
    sys().metrics(spec)
}

/// Draw a shaped line with its baseline starting at `at` (logical), through
/// `t` (logical to device), clipped to `clip` (device) and `mask`.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    pm: &mut PixmapMut<'_>,
    t: Transform,
    line: &Line,
    at: Point,
    color: Color,
    opacity: f32,
    clip: Option<IRect>,
    mask: Option<&Mask>,
) {
    if line.glyphs.is_empty() || opacity <= 0.0 || color.a == 0 {
        return;
    }
    let scale = t.sx;
    let mut s = sys();
    let bounds = IRect::new(0, 0, pm.width() as i32, pm.height() as i32);
    let clip = clip.map(|c| c.intersected(&bounds)).unwrap_or(bounds);
    for g in &line.glyphs {
        let lx = at.x + g.x;
        let ly = at.y + g.y;
        let dx = lx * t.sx + t.tx;
        let dy = (ly * t.sy + t.ty).round();
        let fx = dx.floor();
        let frac = (((dx - fx) * 4.0).round() as i32).clamp(0, 4);
        let (fx, frac) = if frac == 4 { (fx + 1.0, 0) } else { (fx, frac as u8) };
        let key = GlyphKey { face: g.face, id: g.id, weight: g.weight, size: (g.px * scale * 64.0).round() as u32, frac };
        let Some(glyph) = s.glyph(key) else { continue };
        if glyph.w == 0 || glyph.h == 0 {
            continue;
        }
        blit_glyph(pm, &glyph, fx as i32 + glyph.left, dy as i32 + glyph.top, color, opacity, clip, mask);
    }
}

#[allow(clippy::too_many_arguments)]
fn blit_glyph(pm: &mut PixmapMut<'_>, g: &Glyph, x: i32, y: i32, color: Color, opacity: f32, clip: IRect, mask: Option<&Mask>) {
    let area = IRect::new(x, y, g.w as i32, g.h as i32).intersected(&clip);
    if area.is_empty() {
        return;
    }
    let width = pm.width() as usize;
    let data = pm.data_mut();
    let op = (opacity * 256.0).round() as u32;
    // Straight colour to premultiplied, in BGRA byte order.
    let ca = (color.a as u32 * op) >> 8;
    let (cb, cg, cr) = (color.b as u32, color.g as u32, color.r as u32);
    let mask_data = mask.map(|m| (m.data(), m.width() as usize));
    for row in area.y..area.y2() {
        let gy = (row - y) as usize;
        for col in area.x..area.x2() {
            let gx = (col - x) as usize;
            let i = gy * g.w as usize + gx;
            let mut cov = g.alpha[i] as u32;
            if cov == 0 {
                continue;
            }
            if let Some((md, mw)) = mask_data {
                cov = cov * md[row as usize * mw + col as usize] as u32 / 255;
                if cov == 0 {
                    continue;
                }
            }
            let o = (row as usize * width + col as usize) * 4;
            let d = &mut data[o..o + 4];
            let (sb, sg, sr, sa) = match &g.color {
                Some(px) => {
                    // A colour glyph: its own pixels (premultiplied), with opacity.
                    let p = &px[i * 4..i * 4 + 4];
                    let k = op;
                    ((p[0] as u32 * k) >> 8, (p[1] as u32 * k) >> 8, (p[2] as u32 * k) >> 8, (p[3] as u32 * k) >> 8)
                }
                None => {
                    let a = ca * cov / 255;
                    (cb * a / 255, cg * a / 255, cr * a / 255, a)
                }
            };
            if sa == 0 {
                continue;
            }
            let inv = 255 - sa;
            d[0] = (sb + (d[0] as u32 * inv + 127) / 255).min(255) as u8;
            d[1] = (sg + (d[1] as u32 * inv + 127) / 255).min(255) as u8;
            d[2] = (sr + (d[2] as u32 * inv + 127) / 255).min(255) as u8;
            d[3] = (sa + (d[3] as u32 * inv + 127) / 255).min(255) as u8;
        }
    }
}

/// The outlines of a line as one path (logical coordinates), baseline at
/// `at`: text that scales with what it's drawn on, as QPainterPath.addText.
/// Colour glyphs (emoji) have no outline and are returned separately as
/// (glyph, logical rect) to draw as pictures.
pub fn outline_path(line: &Line, at: Point) -> (Option<tiny_skia::Path>, Vec<(Arc<Glyph>, crate::geom::Rect)>) {
    let mut s = sys();
    let mut pb = PathBuilder::new();
    let mut pictures = Vec::new();
    for g in &line.glyphs {
        let face = &s.faces[g.face as usize];
        let Some(font) = face.font_ref() else { continue };
        let loc = face.wght_for(g.weight).map(|v| font.axes().location([("wght", v)]));
        let loc_ref: LocationRef<'_> = loc.as_ref().map(LocationRef::from).unwrap_or_default();
        let outlines = font.outline_glyphs();
        if let Some(outline) = outlines.get(GlyphId::new(g.id)) {
            let mut pen = OffsetPen { pb: &mut pb, x: at.x + g.x, y: at.y + g.y };
            let _ = outline.draw(DrawSettings::unhinted(Size::new(g.px), loc_ref), &mut pen);
            continue;
        }
        // A picture glyph, rendered at 4x the logical size for sharpness.
        let key = GlyphKey { face: g.face, id: g.id, weight: g.weight, size: (g.px * 4.0 * 64.0) as u32, frac: 0 };
        drop(font);
        if let Some(glyph) = s.glyph(key)
            && glyph.color.is_some()
        {
            let r = crate::geom::Rect::new(
                at.x + g.x + glyph.left as f32 / 4.0,
                at.y + g.y + glyph.top as f32 / 4.0,
                glyph.w as f32 / 4.0,
                glyph.h as f32 / 4.0,
            );
            pictures.push((glyph, r));
        }
    }
    (pb.finish(), pictures)
}

/// `text` shortened in the middle with "…" to fit `max` (elidedText).
pub fn elide_middle(text: &str, spec: FontSpec, max: f32) -> String {
    if width(text, spec) <= max {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut keep = chars.len();
    while keep > 0 {
        keep -= 1;
        let head = keep.div_ceil(2);
        let tail = keep / 2;
        let s: String = chars[..head].iter().chain(['…'].iter()).chain(chars[chars.len() - tail..].iter()).collect();
        if width(&s, spec) <= max {
            return s;
        }
    }
    "…".into()
}

/// `text` shortened at the end with "…" to fit `max`.
pub fn elide_end(text: &str, spec: FontSpec, max: f32) -> String {
    if width(text, spec) <= max {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let s: String = chars[..mid].iter().chain(['…'].iter()).collect();
        if width(&s, spec) <= max { lo = mid } else { hi = mid - 1 }
    }
    chars[..lo].iter().chain(['…'].iter()).collect()
}

/// Break `text` into lines no wider than `max` (at spaces where it can).
pub fn wrap(text: &str, spec: FontSpec, max: f32) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut cur = String::new();
        for word in para.split(' ') {
            let candidate = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
            if cur.is_empty() || width(&candidate, spec) <= max {
                cur = candidate;
            } else {
                out.push(std::mem::take(&mut cur));
                cur = word.to_string();
            }
            // A single word too long for a line: break it by characters.
            while width(&cur, spec) > max && cur.chars().count() > 1 {
                let chars: Vec<char> = cur.chars().collect();
                let mut n = chars.len() - 1;
                while n > 1 && width(&chars[..n].iter().collect::<String>(), spec) > max {
                    n -= 1;
                }
                out.push(chars[..n].iter().collect());
                cur = chars[n..].iter().collect();
            }
        }
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_and_measures() {
        let spec = font(12.0, MEDIUM);
        let w = width("Drag to capture", spec);
        assert!(w > 60.0 && w < 140.0, "{w}");
        assert!(width("WWW", spec) > width("iii", spec));
        let m = metrics(spec);
        assert!(m.ascent > 8.0 && m.ascent < 14.0, "{m:?}");
        assert!(m.descent > 1.0 && m.descent < 6.0, "{m:?}");
        assert_eq!(shape("", spec).glyphs.len(), 0);
        // Fallback: CJK and Arabic get glyphs from somewhere.
        assert!(width("漢字", spec) > 10.0);
        assert!(width("مرحبا", spec) > 10.0);
    }

    #[test]
    fn eliding_and_wrapping() {
        let spec = font(12.0, MEDIUM);
        let long = "https://github.com/nicholas-sbx/screenshot/blob/main/README.md";
        let e = elide_middle(long, spec, 120.0);
        assert!(e.contains('…') && width(&e, spec) <= 120.0, "{e}");
        let e = elide_end(long, spec, 100.0);
        assert!(e.ends_with('…') && width(&e, spec) <= 100.0);
        let lines = wrap("one two three four five six seven eight nine ten", spec, 80.0);
        assert!(lines.len() > 2);
        assert!(lines.iter().all(|l| width(l, spec) <= 80.0 || !l.contains(' ')));
    }
}
