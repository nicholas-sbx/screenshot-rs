//! The floating toolbar over the capture: tools, colours, size, toggles,
//! undo, pin, whole screen and cancel. Drag it by its grip, at the left
//! end.

use std::ops::Range;

use crate::geom::{Point, Rect};
use crate::keys::Keymap;
use crate::render::Painter;
use crate::render::theme::{Color, REC};
use crate::ui::shapes::{TOOLS, TextStyle, Tool};
use crate::ui::state::swatch_items;
use crate::ui::widgets::{Item, Look, Pointer, layout_row, paint_item, paint_panel};

/// What the toolbar shows: the session's state, as far as the toolbar cares.
pub struct View<'a> {
    pub tool: Tool,
    /// how the text tool's texts look (its icon)
    pub text_style: TextStyle,
    pub can_record: bool,
    /// the toolbar's colours
    pub palette: &'a [Color],
    pub color_index: usize,
    pub custom_color: Color,
    pub size: usize,
    pub codes_visible: bool,
    pub codes: CodeScan,
    pub snap: bool,
    pub pointer_available: bool,
    pub show_pointer: bool,
    pub can_undo: bool,
    pub can_redo: bool,
    pub pin_mode: bool,
    /// Beautify after capture (the setting)
    pub beautify: bool,
    pub keymap: &'a Keymap,
}

/// How the search for codes stands, for the codes button's tooltip.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CodeScan {
    /// hidden at the start, so the capture hasn't been looked through
    NotYet,
    Scanning,
    Found(usize),
}

#[derive(Clone, Debug, Default)]
pub struct Toolbar {
    /// top left, in the overlay's logical coordinates
    pub pos: Point,
    pub placed: bool,
    pub items: Vec<Item>,
    pub size: (f32, f32),
    pub ptr: Pointer,
    /// where it was grabbed (relative to its top left), while dragged
    pub grab: Option<Point>,
}

/// The space round its controls (left and top; the right has 8).
const PAD: f32 = 6.0;
/// A row's height, and the space between rows.
const ROW: f32 = 36.0;
const ROW_GAP: f32 = 4.0;
/// Between controls in a row.
const GAP: f32 = 2.0;
/// The least space between it and the edges of its monitor.
pub const MARGIN: f32 = 8.0;

impl Toolbar {
    pub fn new(view: &View<'_>) -> Toolbar {
        Toolbar::fitted(view, f32::INFINITY)
    }

    /// Made for an overlay `width` wide: one row, or where that's too wide
    /// (a narrow or upright monitor, many colours) rows broken where its
    /// dividers are, as evenly as they can be, the grip down their left.
    pub fn fitted(view: &View<'_>, width: f32) -> Toolbar {
        let mut t = Toolbar { items: build(view), ..Default::default() };
        t.size = layout_rows(&mut t.items, width - 2.0 * MARGIN);
        t
    }

    /// Bring the controls up to date with `view` (their layout is fixed
    /// once made, as the Python version's).
    pub fn refresh(&mut self, view: &View<'_>) {
        let fresh = build(view);
        for it in self.items.iter_mut() {
            if let Some(f) = fresh.iter().find(|f| f.id == it.id) {
                let rect = it.rect;
                *it = f.clone();
                it.rect = rect;
            }
        }
    }

    pub fn rect(&self) -> Rect {
        Rect::new(self.pos.x, self.pos.y, self.size.0, self.size.1)
    }

    /// Centred at the top of an overlay `width` wide.
    pub fn place(&mut self, width: f32) {
        self.pos = Point::new(((width - self.size.0) / 2.0).floor().max(8.0), 20.0);
        self.placed = true;
    }

    /// Is `at` on the grip it's dragged by: the left end, through the grip's
    /// dots, its full height (the margin round the buttons isn't)?
    pub fn on_grip(&self, at: Point) -> bool {
        let Some(grip) = self.items.iter().find(|it| it.id == "grip") else { return false };
        let local = at - self.pos;
        self.rect().contains(at) && local.x < grip.rect.right()
    }

    pub fn item_at(&self, at: Point) -> Option<&Item> {
        let local = at - self.pos;
        self.items.iter().find(|it| it.rect.contains(local))
    }

    pub fn paint(&self, p: &mut Painter<'_>) {
        paint_panel(p, self.rect(), 14.0);
        for it in &self.items {
            paint_item(p, it, self.pos, &self.ptr);
        }
    }

    /// The hint of the control under the pointer, if any.
    pub fn hint(&self) -> Option<String> {
        let id = self.ptr.hovered?;
        self.items.iter().find(|it| it.id == id).map(|it| it.hint.clone())
    }
}

/// Lay out `items` (the grip first) in as few rows as fit in `room`, and
/// return the toolbar's size. The dividers rows are broken at go.
fn layout_rows(items: &mut Vec<Item>, room: f32) -> (f32, f32) {
    let one = layout_row(items, PAD, PAD, ROW, GAP) + 8.0;
    if one <= room || items.len() < 2 {
        return (one, ROW + 2.0 * PAD);
    }
    let grip = items[0].size().0;
    let left = PAD + grip + GAP;
    let rest: Vec<Item> = items.drain(1..).collect();
    let rows = break_rows(&rest, room - left - 8.0);
    let mut wide = 0.0f32;
    for (i, r) in rows.iter().enumerate() {
        let mut row = rest[r.clone()].to_vec();
        wide = wide.max(layout_row(&mut row, left, PAD + i as f32 * (ROW + ROW_GAP), ROW, GAP));
        items.extend(row);
    }
    let h = rows.len() as f32 * (ROW + ROW_GAP) - ROW_GAP;
    items[0].rect = Rect::new(PAD, PAD, grip, h);
    (wide + 8.0, h + 2.0 * PAD)
}

/// How to break `items` into rows `room` wide at most (ranges of them; a
/// divider broken at is in none): at dividers, the fewest rows and then the
/// narrowest; else, where none of that fits, wherever a row fills up.
fn break_rows(items: &[Item], room: f32) -> Vec<Range<usize>> {
    let width = |r: &Range<usize>| items[r.clone()].iter().map(|it| it.size().0 + GAP).sum::<f32>() - GAP;
    let dividers: Vec<usize> = items.iter().enumerate().filter(|(_, it)| it.look == Look::Divider).map(|(i, _)| i).take(12).collect();
    let mut best: Option<(usize, f32, Vec<Range<usize>>)> = None;
    for breaks in 1u32..1 << dividers.len() {
        let mut rows = Vec::new();
        let mut from = 0;
        for (j, &d) in dividers.iter().enumerate() {
            if breaks & (1 << j) != 0 {
                rows.push(from..d);
                from = d + 1;
            }
        }
        rows.push(from..items.len());
        let widest = rows.iter().map(width).fold(0.0, f32::max);
        if widest <= room && best.as_ref().is_none_or(|b| (rows.len(), widest) < (b.0, b.1)) {
            best = Some((rows.len(), widest, rows));
        }
    }
    if let Some((_, _, rows)) = best {
        return rows;
    }
    let divider = |i: usize| items[i].look == Look::Divider;
    let mut rows = Vec::new();
    let (mut from, mut i) = (0, 0);
    while i < items.len() {
        if i > from && width(&(from..i + 1)) > room {
            // (at its last divider, if that leaves it at least half full)
            let at = (from + 1..i).rev().find(|&d| divider(d)).filter(|&d| width(&(from..d)) >= room / 2.0);
            rows.push(from..at.unwrap_or(i));
            from = at.map_or(i, |d| d + 1);
            continue;
        }
        i += 1;
    }
    rows.push(from..items.len());
    // (no row starts or ends with a divider)
    for r in &mut rows {
        while r.start < r.end && divider(r.start) {
            r.start += 1;
        }
        while r.start < r.end && divider(r.end - 1) {
            r.end -= 1;
        }
    }
    rows.retain(|r| r.start < r.end);
    rows
}

fn build(v: &View<'_>) -> Vec<Item> {
    let km = v.keymap;
    let recording = v.tool == Tool::Record;
    let mut items = vec![Item::new("grip", Look::Grip, "")];
    for (tool, id, label) in TOOLS.iter().filter(|t| t.0 != Tool::Crop) {
        if *tool == Tool::Record && !v.can_record {
            continue;
        }
        // (the text tool in its style: chosen again, its next)
        let (icon, label) = if *tool == Tool::Text { (v.text_style.icon(), v.text_style.hint()) } else { (*id, *label) };
        let mut hint = km.hint(label, &format!("tool.{id}"));
        if *tool == Tool::Text {
            hint.push_str("  ·  again for another style");
        }
        let mut b = Item::new(id, Look::Icon { icon, size: 36.0 }, hint);
        b.active = *tool == v.tool;
        b.dimmed = recording && !tool.picks_area();
        if *tool == Tool::Record {
            b.tint = Some(REC);
        }
        items.push(b);
        if *tool == Tool::Ocr {
            items.push(Item::new("divider.modes", Look::Divider, ""));
        }
    }
    items.push(Item::new("divider.tools", Look::Divider, ""));
    items.extend(swatch_items(km, v.palette, v.color_index, v.custom_color, !recording));
    let keys: Vec<String> = [km.label("size.down"), km.label("size.up")].into_iter().filter(|k| !k.is_empty()).collect();
    let mut size =
        Item::new("size", Look::Size(v.size), if keys.is_empty() { "Stroke size".to_string() } else { format!("Stroke size  ·  {}", keys.join(" ")) });
    size.enabled = !recording;
    items.push(size);
    items.push(Item::new("divider.colors", Look::Divider, ""));

    let found = match v.codes {
        CodeScan::NotYet => "Not looked for yet".to_string(),
        CodeScan::Scanning => "Looking for codes…".to_string(),
        CodeScan::Found(0) => "No codes found".to_string(),
        CodeScan::Found(n) => format!("{n} code{} found", if n != 1 { "s" } else { "" }),
    };
    // (on or off as the magnet is: the same icon, lit while on)
    let mut codes = Item::new(
        "codes",
        Look::Icon { icon: "codes", size: 36.0 },
        km.hint("Show QR / barcodes", "codes") + if v.codes_visible { "  ·  on" } else { "  ·  off" } + &format!("  ·  {found}"),
    );
    codes.toggled = Some(v.codes_visible);
    codes.enabled = !recording;
    items.push(codes);
    let mut snap = Item::new(
        "snap",
        Look::Icon { icon: "magnet", size: 36.0 },
        km.hint("Snap selections to edges in the picture", "snap") + "  ·  " + if v.snap { "on (hold Ctrl to place freely)" } else { "off" },
    );
    snap.toggled = Some(v.snap);
    items.push(snap);
    if v.pointer_available {
        let shown = v.show_pointer;
        let mut pointer = Item::new(
            "pointer",
            Look::Icon { icon: if shown { "cursor" } else { "cursor-off" }, size: 36.0 },
            km.hint(if shown { "Hide the mouse pointer" } else { "Show the mouse pointer" }, "pointer") + if shown { "  ·  shown in the capture" } else { "" },
        );
        pointer.toggled = Some(shown);
        pointer.enabled = !recording;
        items.push(pointer);
    }
    let mut undo = Item::new("undo", Look::Icon { icon: "undo", size: 36.0 }, km.hint("Undo", "undo"));
    undo.enabled = !recording && v.can_undo;
    items.push(undo);
    let mut redo = Item::new("redo", Look::Icon { icon: "redo", size: 36.0 }, km.hint("Redo", "redo"));
    redo.enabled = !recording && v.can_redo;
    items.push(redo);
    items.push(Item::new("divider.end", Look::Divider, ""));
    let mut beautify = Item::new(
        "beautify",
        Look::Icon { icon: "beautify", size: 36.0 },
        km.hint(if v.beautify { "Beautify is on" } else { "Beautify: on a background" }, "beautify"),
    );
    beautify.toggled = Some(v.beautify);
    beautify.enabled = !recording;
    items.push(beautify);
    let mut pin = Item::new(
        "pin",
        Look::Icon { icon: "pin", size: 36.0 },
        km.hint(if v.pin_mode { "Pinning: the capture stays on screen, not saved" } else { "Pin the capture to the screen instead of saving it" }, "pin"),
    );
    pin.active = v.pin_mode;
    pin.enabled = !recording;
    items.push(pin);
    items.push(Item::new(
        "screen",
        Look::Icon { icon: "screen", size: 36.0 },
        if recording { "Record whole screen  ·  Enter" } else { "Capture whole screen  ·  Enter" },
    ));
    items.push(Item::new("close", Look::Icon { icon: "close", size: 36.0 }, "Cancel  ·  Esc"));
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn view<'a>(km: &'a Keymap, palette: &'a [Color]) -> View<'a> {
        View {
            tool: Tool::Region,
            text_style: TextStyle::Plain,
            can_record: true,
            palette,
            color_index: 0,
            custom_color: Color::rgb(0xFF4FA3),
            size: 1,
            codes_visible: true,
            codes: CodeScan::Found(2),
            snap: false,
            pointer_available: false,
            show_pointer: false,
            can_undo: false,
            can_redo: false,
            pin_mode: false,
            beautify: false,
            keymap: km,
        }
    }

    #[test]
    fn layout_like_qt() {
        let km = Keymap::new(&BTreeMap::new());
        let t = Toolbar::new(&view(&km, &crate::render::theme::SWATCHES));
        // grip 18, 17 tools of 36, 4 dividers of 13, 8 swatches of 26,
        // size 40, codes + snap + undo + redo + beautify + pin + screen + close (8 x 36)
        let widths = 18.0 + 17.0 * 36.0 + 4.0 * 13.0 + 8.0 * 26.0 + 40.0 + 8.0 * 36.0;
        let n = 1 + 17 + 4 + 8 + 1 + 8;
        assert_eq!(t.size, (6.0 + widths + 2.0 * (n as f32 - 1.0) + 8.0, 48.0));
        let undo = t.items.iter().find(|i| i.id == "undo").unwrap();
        assert!(!undo.enabled);
        assert!(t.items.iter().find(|i| i.id == "region").unwrap().active);
        assert_eq!(t.items.iter().find(|i| i.id == "codes").unwrap().hint, "Show QR / barcodes  ·  Q  ·  on  ·  2 codes found");
        assert_eq!(t.items.iter().find(|i| i.id == "swatch.2").unwrap().hint, "Colour  ·  3");
    }

    #[test]
    fn as_wide_as_its_colours() {
        let km = Keymap::new(&BTreeMap::new());
        let seven = Toolbar::new(&view(&km, &crate::render::theme::SWATCHES)).size.0;
        let ten = [Color::rgb(0x112233); 10];
        let t = Toolbar::new(&view(&km, &ten));
        // three more swatches of 26, with their gaps of 2
        assert_eq!(t.size.0, seven + 3.0 * 28.0);
        assert_eq!(t.items.iter().filter(|i| matches!(i.look, Look::Swatch(_))).count(), 10);
        assert_eq!(t.items.iter().find(|i| i.id == "swatch.9").unwrap().hint, "Colour  ·  0");
        let two = Toolbar::new(&view(&km, &ten[..2])).size.0;
        assert_eq!(two, seven - 5.0 * 28.0);
    }

    #[test]
    fn in_rows_where_one_is_too_wide() {
        let km = Keymap::new(&BTreeMap::new());
        let v = view(&km, &crate::render::theme::SWATCHES);
        let one = Toolbar::new(&v);
        let clickable = |t: &Toolbar| t.items.iter().filter(|i| i.clickable()).count();
        // On a monitor 1280 wide (1920 at 150 %): two rows, the tools over the rest.
        let t = Toolbar::fitted(&v, 1280.0);
        assert!(t.size.0 <= 1280.0 - 2.0 * MARGIN, "{:?}", t.size);
        assert_eq!(t.size.1, 88.0);
        let top = |id: &str| t.items.iter().find(|i| i.id == id).unwrap().rect.y;
        assert_eq!(top("region"), top("counter"));
        assert!(top("swatch.1") > top("counter") && top("close") == top("swatch.1"));
        assert!(t.items.iter().all(|i| i.id != "divider.tools"));
        // All the rest still there, inside it, none on another; the grip down its left.
        assert_eq!(t.items.len(), one.items.len() - 1);
        let bounds = Rect::new(0.0, 0.0, t.size.0, t.size.1);
        for (i, a) in t.items.iter().enumerate() {
            assert!(bounds.contains_rect(&a.rect), "{} at {:?}", a.id, a.rect);
            assert!(t.items[i + 1..].iter().all(|b| !a.rect.intersects(&b.rect)), "{} overlaps", a.id);
        }
        assert_eq!(t.items[0].rect.h, 76.0);
        assert!(t.on_grip(Point::new(10.0, 70.0)));
        // Room for one row: one, as it always was.
        let wide = Toolbar::fitted(&v, one.size.0 + 2.0 * MARGIN);
        assert_eq!((wide.size, wide.items.len()), (one.size, one.items.len()));
        // Narrower than some of its groups: as many rows as it takes.
        let narrow = Toolbar::fitted(&v, 420.0);
        assert!(narrow.size.0 <= 420.0 - 2.0 * MARGIN && narrow.size.1 > 88.0, "{:?}", narrow.size);
        assert_eq!(clickable(&narrow), clickable(&one));
    }
}
