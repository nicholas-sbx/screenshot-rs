//! The floating toolbar over the capture: tools, colours, size, toggles,
//! undo, pin, whole screen and cancel. Drag it by anything that isn't a
//! button.

use crate::geom::{Point, Rect};
use crate::keys::Keymap;
use crate::render::Painter;
use crate::render::theme::{Color, REC};
use crate::ui::shapes::{TOOLS, Tool};
use crate::ui::widgets::{Item, Look, Pointer, layout_row, paint_item, paint_panel};

/// What the toolbar shows: the session's state, as far as the toolbar cares.
pub struct View<'a> {
    pub tool: Tool,
    pub can_record: bool,
    pub color_index: usize,
    pub custom_color: Color,
    pub size: usize,
    pub codes_visible: bool,
    pub code_count: usize,
    pub snap: bool,
    pub pointer_available: bool,
    pub show_pointer: bool,
    pub can_undo: bool,
    pub can_redo: bool,
    pub pin_mode: bool,
    pub keymap: &'a Keymap,
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

const SWATCH_COUNT: usize = 7;

impl Toolbar {
    pub fn new(view: &View<'_>) -> Toolbar {
        let mut t = Toolbar::default();
        t.items = build(view);
        let w = layout_row(&mut t.items, 6.0, 6.0, 36.0, 2.0) + 8.0;
        t.size = (w, 48.0);
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

fn build(v: &View<'_>) -> Vec<Item> {
    let km = v.keymap;
    let recording = v.tool == Tool::Record;
    let mut items = vec![Item::new("grip", Look::Grip, "")];
    for (tool, id, label) in TOOLS {
        if *tool == Tool::Record && !v.can_record {
            items.push(Item::new("divider.record", Look::Divider, ""));
            continue;
        }
        let mut b = Item::new(id, Look::Icon { icon: id, size: 36.0 }, km.hint(label, &format!("tool.{id}")));
        b.active = *tool == v.tool;
        b.dimmed = recording && !matches!(tool, Tool::Region | Tool::Record);
        if *tool == Tool::Record {
            b.tint = Some(REC);
        }
        items.push(b);
        if *tool == Tool::Record {
            items.push(Item::new("divider.record", Look::Divider, ""));
        }
    }
    items.push(Item::new("divider.tools", Look::Divider, ""));
    const SWATCH_IDS: [&str; 8] = ["color.1", "color.2", "color.3", "color.4", "color.5", "color.6", "color.7", "color.8"];
    for (i, id) in SWATCH_IDS.iter().enumerate().take(SWATCH_COUNT) {
        let mut s = Item::new(id, Look::Swatch(i), km.hint("Colour", id));
        s.selected = v.color_index == i;
        s.enabled = !recording;
        items.push(s);
    }
    let mut custom = Item::new(
        "color.8",
        Look::CustomSwatch(v.custom_color),
        km.hint("Your own colour", "color.8") + "  ·  click to choose it",
    );
    custom.selected = v.color_index == SWATCH_COUNT;
    custom.enabled = !recording;
    items.push(custom);
    let keys: Vec<String> = [km.label("size.down"), km.label("size.up")].into_iter().filter(|k| !k.is_empty()).collect();
    let mut size = Item::new(
        "size",
        Look::Size(v.size),
        if keys.is_empty() { "Stroke size".to_string() } else { format!("Stroke size  ·  {}", keys.join(" ")) },
    );
    size.enabled = !recording;
    items.push(size);
    items.push(Item::new("divider.colors", Look::Divider, ""));

    let found =
        if v.code_count > 0 { format!("{} code{} found", v.code_count, if v.code_count != 1 { "s" } else { "" }) } else { "No codes found".into() };
    let mut codes = if v.codes_visible {
        Item::new("codes", Look::Icon { icon: "codes", size: 36.0 }, km.hint("Hide QR / barcodes", "codes") + &format!("  ·  {found}"))
    } else {
        Item::new("codes", Look::Icon { icon: "codes-off", size: 36.0 }, km.hint("Show QR / barcodes", "codes") + &format!("  ·  {found}"))
    };
    codes.toggled = Some(v.codes_visible);
    codes.enabled = !recording;
    items.push(codes);
    let mut snap = Item::new(
        "snap",
        Look::Icon { icon: "magnet", size: 36.0 },
        km.hint("Snap selections to edges in the picture", "snap")
            + "  ·  "
            + if v.snap { "on (hold Ctrl to place freely)" } else { "off" },
    );
    snap.toggled = Some(v.snap);
    items.push(snap);
    if v.pointer_available {
        let shown = v.show_pointer;
        let mut pointer = Item::new(
            "pointer",
            Look::Icon { icon: if shown { "cursor" } else { "cursor-off" }, size: 36.0 },
            km.hint(if shown { "Hide the mouse pointer" } else { "Show the mouse pointer" }, "pointer")
                + if shown { "  ·  shown in the capture" } else { "" },
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
    let mut pin = Item::new(
        "pin",
        Look::Icon { icon: "pin", size: 36.0 },
        km.hint(
            if v.pin_mode { "Pinning: the capture stays on screen, not saved" } else { "Pin the capture to the screen instead of saving it" },
            "pin",
        ),
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

    fn view(km: &Keymap) -> View<'_> {
        View {
            tool: Tool::Region,
            can_record: true,
            color_index: 0,
            custom_color: Color::rgb(0xFF4FA3),
            size: 1,
            codes_visible: true,
            code_count: 2,
            snap: false,
            pointer_available: false,
            show_pointer: false,
            can_undo: false,
            can_redo: false,
            pin_mode: false,
            keymap: km,
        }
    }

    #[test]
    fn layout_like_qt() {
        let km = Keymap::new(&BTreeMap::new());
        let t = Toolbar::new(&view(&km));
        // grip 18, 13 tools of 36, 4 dividers of 13, 8 swatches of 26,
        // size 40, codes + snap + undo + redo + pin + screen + close (7 x 36)
        let widths = 18.0 + 13.0 * 36.0 + 4.0 * 13.0 + 8.0 * 26.0 + 40.0 + 7.0 * 36.0;
        let n = 1 + 13 + 4 + 8 + 1 + 7;
        assert_eq!(t.size, (6.0 + widths + 2.0 * (n as f32 - 1.0) + 8.0, 48.0));
        let undo = t.items.iter().find(|i| i.id == "undo").unwrap();
        assert!(!undo.enabled);
        assert!(t.items.iter().find(|i| i.id == "region").unwrap().active);
        assert_eq!(t.items.iter().find(|i| i.id == "codes").unwrap().hint, "Hide QR / barcodes  ·  Q  ·  2 codes found");
    }
}
