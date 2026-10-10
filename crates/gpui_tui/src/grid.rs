use std::{fmt, io};

use gpui::{GlyphId, Hsla, Rgba};

use crate::text_system::char_cells;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn distance(self, other: Rgb) -> u32 {
        self.r.abs_diff(other.r) as u32
            + self.g.abs_diff(other.g) as u32
            + self.b.abs_diff(other.b) as u32
    }

    pub fn blend(self, color: Hsla) -> Self {
        self.blend_rgba(color.to_rgb())
    }

    pub fn blend_rgba(self, rgba: Rgba) -> Self {
        let alpha = rgba.a.clamp(0., 1.);
        let mix = |under: u8, over: f32| {
            let over = (over.clamp(0., 1.) * 255.).round();
            (under as f32 * (1. - alpha) + over * alpha).round() as u8
        };
        Self {
            r: mix(self.r, rgba.r),
            g: mix(self.g, rgba.g),
            b: mix(self.b, rgba.b),
        }
    }
}

impl From<Rgb> for u32 {
    fn from(rgb: Rgb) -> Self {
        (rgb.r as u32) << 16 | (rgb.g as u32) << 8 | rgb.b as u32
    }
}

impl From<u32> for Rgb {
    fn from(color: u32) -> Self {
        Rgb::new((color >> 16) as u8, (color >> 8) as u8, color as u8)
    }
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct CellAttrs: u8 {
        const BOLD = 1;
        const ITALIC = 1 << 1;
        const UNDERLINE = 1 << 2;
        const WIDE_CONTINUATION = 1 << 4;
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Glyph(u32);

impl Glyph {
    pub const fn from_char(ch: char) -> Self {
        Self(ch as u32)
    }

    pub fn from_glyph_id(id: GlyphId) -> Option<Self> {
        let glyph = Self(id.0);
        glyph.as_char().is_some().then_some(glyph)
    }

    pub fn to_glyph_id(self) -> GlyphId {
        GlyphId(self.0)
    }

    pub fn to_u32(self) -> u32 {
        self.0
    }

    pub fn as_char(self) -> Option<char> {
        char::from_u32(self.0)
    }

    pub fn cells(self) -> usize {
        self.as_char().map_or(1, char_cells)
    }

    pub fn with_str<R>(self, f: impl FnOnce(&str) -> R) -> R {
        match self.as_char() {
            Some(ch) => f(ch.encode_utf8(&mut [0; 4])),
            None => f(" "),
        }
    }

    pub fn push_to(self, text: &mut String) {
        self.with_str(|glyph| text.push_str(glyph))
    }

    pub fn write_to(self, output: &mut impl io::Write) -> io::Result<()> {
        self.with_str(|text| output.write_all(text.as_bytes()))
    }
}

impl From<char> for Glyph {
    fn from(ch: char) -> Self {
        Self::from_char(ch)
    }
}

impl PartialEq<char> for Glyph {
    fn eq(&self, ch: &char) -> bool {
        self.0 == *ch as u32
    }
}

impl fmt::Debug for Glyph {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.with_str(|text| fmt::Debug::fmt(text, formatter))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub glyph: Glyph,
    pub fg: Rgb,
    pub bg: Rgb,
    pub attrs: CellAttrs,
}

impl Cell {
    pub fn is_wide_continuation(&self) -> bool {
        self.attrs.contains(CellAttrs::WIDE_CONTINUATION)
    }

    pub fn blank(bg: Rgb) -> Self {
        Self {
            glyph: Glyph::from_char(' '),
            fg: Rgb::new(255, 255, 255),
            bg,
            attrs: CellAttrs::empty(),
        }
    }

    pub fn is_plain_blank(&self) -> bool {
        self.glyph == ' '
            && !self
                .attrs
                .intersects(CellAttrs::UNDERLINE | CellAttrs::WIDE_CONTINUATION)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellGrid {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Cell>,
}

impl CellGrid {
    pub fn new(cols: u16, rows: u16, background: Rgb) -> Self {
        Self {
            cols,
            rows,
            cells: vec![Cell::blank(background); cols as usize * rows as usize],
        }
    }

    pub fn index(&self, col: i32, row: i32) -> Option<usize> {
        if col < 0 || row < 0 || col >= self.cols as i32 || row >= self.rows as i32 {
            return None;
        }
        Some(row as usize * self.cols as usize + col as usize)
    }

    pub fn cell(&self, col: i32, row: i32) -> Option<&Cell> {
        self.index(col, row).and_then(|index| self.cells.get(index))
    }

    pub fn row_mut(&mut self, row: u16) -> &mut [Cell] {
        let start = row as usize * self.cols as usize;
        let end = start + self.cols as usize;
        self.cells.get_mut(start..end).unwrap_or(&mut [])
    }

    pub fn cell_mut(&mut self, col: i32, row: i32) -> Option<&mut Cell> {
        self.index(col, row)
            .and_then(|index| self.cells.get_mut(index))
    }

    pub fn row(&self, row: u16) -> &[Cell] {
        let start = row as usize * self.cols as usize;
        let end = start + self.cols as usize;
        self.cells.get(start..end).unwrap_or(&[])
    }

    pub fn row_text(&self, row: u16) -> String {
        self.row(row)
            .iter()
            .filter(|cell| !cell.is_wide_continuation())
            .fold(String::new(), |mut text, cell| {
                cell.glyph.push_to(&mut text);
                text
            })
    }

    pub fn text(&self) -> String {
        (0..self.rows)
            .map(|row| self.row_text(row).trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) fn split_wide_char_at(&mut self, col: i32, row: i32) {
        let is_continuation = self
            .cell(col, row)
            .is_some_and(|cell| cell.is_wide_continuation());
        if !is_continuation {
            return;
        }
        if let Some(lead) = self.cell_mut(col - 1, row) {
            lead.glyph = ' '.into();
        }
        if let Some(continuation) = self.cell_mut(col, row) {
            continuation.glyph = ' '.into();
            continuation.attrs.remove(CellAttrs::WIDE_CONTINUATION);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blending_respects_alpha() {
        let base = Rgb::new(0, 0, 0);
        assert_eq!(
            base.blend(gpui::opaque_grey(1., 0.5)),
            Rgb::new(128, 128, 128)
        );
    }
}
