//! Drawing: the palette, a painter over tiny-skia, text and icons.

pub mod icons;
pub mod paint;
pub mod text;
pub mod theme;

pub use paint::{Align, Cap, Join, Painter, Pen};
pub use text::{BOLD, DEMIBOLD, FontSpec, MEDIUM, NORMAL, font, mono};
pub use theme::{Color, c};
