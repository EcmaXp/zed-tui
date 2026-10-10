use std::{
    fmt, io,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
    },
};

use collections::HashMap;
use gpui::{GlyphId, Hsla, Rgba};
use parking_lot::RwLock;

use crate::text_system::{char_cells, cluster_cells};

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
        const CURLY_UNDERLINE = 1 << 3;
        const WIDE_CONTINUATION = 1 << 4;
        const DEFAULT_BACKGROUND = 1 << 5;
        const DEFAULT_FOREGROUND = 1 << 6;
    }
}

const FIRST_CLUSTER_ID: u32 = 0x11_0000;
const MAX_UNDERLINE_COLORS: usize = u8::MAX as usize;

#[derive(Default)]
struct ClusterTable {
    ids: HashMap<Arc<str>, u32>,
    clusters: Vec<(Arc<str>, u8)>,
}

static CLUSTERS: LazyLock<RwLock<ClusterTable>> = LazyLock::new(RwLock::default);
static UNDERLINE_COLORS: LazyLock<RwLock<Vec<Rgb>>> = LazyLock::new(RwLock::default);
static UNDERLINE_COLORS_FULL: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Glyph(u32);

impl Glyph {
    pub const fn from_char(ch: char) -> Self {
        Self(ch as u32)
    }

    pub fn from_cluster(cluster: &str) -> Self {
        let mut chars = cluster.chars();
        let Some(first) = chars.next() else {
            return Self::from_char(' ');
        };
        if chars.next().is_none() {
            return Self::from_char(first);
        }
        if let Some(id) = CLUSTERS.read().ids.get(cluster) {
            return Self(*id);
        }
        let mut table = CLUSTERS.write();
        if let Some(id) = table.ids.get(cluster) {
            return Self(*id);
        }
        let Some(id) = u32::try_from(table.clusters.len())
            .ok()
            .and_then(|index| FIRST_CLUSTER_ID.checked_add(index))
        else {
            return Self::from_char(first);
        };
        let cells = cluster_cells(cluster).max(1) as u8;
        let cluster: Arc<str> = cluster.into();
        table.clusters.push((cluster.clone(), cells));
        table.ids.insert(cluster, id);
        Self(id)
    }

    pub fn from_glyph_id(id: GlyphId) -> Option<Self> {
        let glyph = Self(id.0);
        (glyph.as_char().is_some() || glyph.cluster_cells().is_some()).then_some(glyph)
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
        match self.as_char() {
            Some(ch) => char_cells(ch),
            None => self.cluster_cells().map_or(1, usize::from),
        }
    }

    pub fn with_str<R>(self, f: impl FnOnce(&str) -> R) -> R {
        match self.as_char() {
            Some(ch) => f(ch.encode_utf8(&mut [0; 4])),
            None => match self.cluster() {
                Some((cluster, _)) => f(&cluster),
                None => f(" "),
            },
        }
    }

    pub fn push_to(self, text: &mut String) {
        self.with_str(|glyph| text.push_str(glyph))
    }

    pub fn write_to(self, output: &mut impl io::Write) -> io::Result<()> {
        self.with_str(|text| output.write_all(text.as_bytes()))
    }

    fn cluster(self) -> Option<(Arc<str>, u8)> {
        let index = self.0.checked_sub(FIRST_CLUSTER_ID)?;
        CLUSTERS.read().clusters.get(index as usize).cloned()
    }

    fn cluster_cells(self) -> Option<u8> {
        let index = self.0.checked_sub(FIRST_CLUSTER_ID)?;
        CLUSTERS
            .read()
            .clusters
            .get(index as usize)
            .map(|(_, cells)| *cells)
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct UnderlineColor(u8);

impl UnderlineColor {
    pub fn of(color: Rgb) -> Self {
        let position = |colors: &[Rgb]| colors.iter().position(|known| *known == color);
        if let Some(index) = position(&UNDERLINE_COLORS.read()) {
            return Self::from_index(index);
        }
        let mut colors = UNDERLINE_COLORS.write();
        if let Some(index) = position(&colors) {
            return Self::from_index(index);
        }
        if colors.len() >= MAX_UNDERLINE_COLORS {
            if !UNDERLINE_COLORS_FULL.swap(true, Ordering::Relaxed) {
                log::warn!("underline colors are full; new ones take the text color");
            }
            return Self::default();
        }
        colors.push(color);
        Self::from_index(colors.len() - 1)
    }

    pub fn rgb(self) -> Option<Rgb> {
        let index = usize::from(self.0).checked_sub(1)?;
        UNDERLINE_COLORS.read().get(index).copied()
    }

    fn from_index(index: usize) -> Self {
        u8::try_from(index + 1).map_or(Self::default(), Self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub glyph: Glyph,
    pub fg: Rgb,
    pub bg: Rgb,
    pub attrs: CellAttrs,
    pub underline: UnderlineColor,
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
            underline: UnderlineColor::default(),
        }
    }

    pub fn is_plain_blank(&self) -> bool {
        self.glyph == ' '
            && !self
                .attrs
                .intersects(CellAttrs::UNDERLINE | CellAttrs::WIDE_CONTINUATION)
    }

    pub fn appearance(&self) -> Cell {
        if self.is_plain_blank() {
            Cell {
                fg: Rgb::default(),
                attrs: self.attrs & CellAttrs::DEFAULT_BACKGROUND,
                underline: UnderlineColor::default(),
                ..*self
            }
        } else {
            *self
        }
    }

    pub fn looks_like(&self, other: &Cell) -> bool {
        self == other || self.appearance() == other.appearance()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorPosition {
    pub col: u16,
    pub row: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    #[default]
    Bar,
    Block,
    Underline,
}

#[derive(Debug, PartialEq, Eq)]
pub struct CellGrid {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Cell>,
    pub cursor: Option<CursorPosition>,
    pub cursor_shape: CursorShape,
}

impl Clone for CellGrid {
    fn clone(&self) -> Self {
        Self {
            cols: self.cols,
            rows: self.rows,
            cells: self.cells.clone(),
            cursor: self.cursor,
            cursor_shape: self.cursor_shape,
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.cols = source.cols;
        self.rows = source.rows;
        self.cells.clone_from(&source.cells);
        self.cursor = source.cursor;
        self.cursor_shape = source.cursor_shape;
    }
}

impl CellGrid {
    pub fn new(cols: u16, rows: u16, background: Rgb) -> Self {
        Self {
            cols,
            rows,
            cells: vec![Cell::blank(background); cols as usize * rows as usize],
            cursor: None,
            cursor_shape: CursorShape::default(),
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

    pub fn mark_default_colors(&mut self, backgrounds: &[Rgb], foregrounds: &[Rgb]) {
        for cell in &mut self.cells {
            let default_background = backgrounds.contains(&cell.bg);
            cell.attrs
                .set(CellAttrs::DEFAULT_BACKGROUND, default_background);
            cell.attrs.set(
                CellAttrs::DEFAULT_FOREGROUND,
                default_background && foregrounds.contains(&cell.fg),
            );
        }
    }

    pub fn text(&self) -> String {
        (0..self.rows)
            .map(|row| self.row_text(row).trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) fn overlay_with_left_bar(
        &mut self,
        overlay: &CellGrid,
        origin: CursorPosition,
        bar: Rgb,
    ) {
        let (col, row) = (i32::from(origin.col), i32::from(origin.row));
        let bar_col = col - 1;
        let end_col = col + i32::from(overlay.cols);
        let bar_cell = Cell {
            glyph: '▌'.into(),
            fg: bar,
            bg: overlay.cells.first().map_or(Rgb::default(), |cell| cell.bg),
            attrs: CellAttrs::empty(),
            underline: UnderlineColor::default(),
        };
        for y in row..row + i32::from(overlay.rows) {
            self.split_wide_char_at(bar_col, y);
            self.split_wide_char_at(end_col, y);
            if let Some(target) = self.cell_mut(bar_col, y) {
                *target = bar_cell;
            }
            for x in col..end_col {
                if let Some(cell) = overlay.cell(x - col, y - row)
                    && let Some(target) = self.cell_mut(x, y)
                {
                    *target = *cell;
                }
            }
        }
        self.cursor = overlay.cursor.map(|cursor| CursorPosition {
            col: cursor.col + origin.col,
            row: cursor.row + origin.row,
        });
        self.cursor_shape = overlay.cursor_shape;
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
    fn cells_keep_their_size_with_an_underline_color() {
        assert_eq!(std::mem::size_of::<Cell>(), 12);
    }

    #[test]
    fn underline_colors_round_trip_through_the_table() {
        let color = Rgb::new(224, 108, 117);
        let underline = UnderlineColor::of(color);
        assert_ne!(underline, UnderlineColor::default());
        assert_eq!(underline, UnderlineColor::of(color));
        assert_eq!(underline.rgb(), Some(color));
        assert_eq!(UnderlineColor::default().rgb(), None);
    }

    #[test]
    fn blanks_that_differ_only_in_invisible_attributes_look_alike() {
        let editor = Rgb::new(40, 44, 51);
        let mut grid = CellGrid::new(2, 1, editor);
        if let Some(cell) = grid.cell_mut(1, 0) {
            cell.fg = Rgb::new(1, 2, 3);
            cell.attrs = CellAttrs::BOLD | CellAttrs::ITALIC;
        }
        grid.mark_default_colors(&[editor], &[Rgb::new(255, 255, 255)]);
        let row = grid.row(0);
        assert!(row[0].attrs.contains(CellAttrs::DEFAULT_FOREGROUND));
        assert!(!row[1].attrs.contains(CellAttrs::DEFAULT_FOREGROUND));
        assert!(row[0].looks_like(&row[1]));

        let mut underlined = row[1];
        underlined.attrs.insert(CellAttrs::UNDERLINE);
        assert!(!row[0].looks_like(&underlined));
        let mut other_background = row[1];
        other_background.attrs.remove(CellAttrs::DEFAULT_BACKGROUND);
        assert!(!row[0].looks_like(&other_background));
    }

    #[test]
    fn default_text_color_is_only_used_on_default_backgrounds() {
        let editor = Rgb::new(40, 44, 51);
        let popup = Rgb::new(47, 52, 62);
        let text = Rgb::new(220, 223, 228);
        let mut grid = CellGrid::new(2, 1, editor);
        for cell in grid.row_mut(0) {
            cell.fg = text;
        }
        if let Some(cell) = grid.cell_mut(1, 0) {
            cell.bg = popup;
        }
        grid.mark_default_colors(&[editor], &[text]);
        let flagged: Vec<bool> = grid
            .row(0)
            .iter()
            .map(|cell| cell.attrs.contains(CellAttrs::DEFAULT_FOREGROUND))
            .collect();
        assert_eq!(flagged, vec![true, false]);
    }

    #[test]
    fn mark_default_background_flags_only_matching_cells() {
        let editor = Rgb::new(40, 44, 51);
        let mut grid = CellGrid::new(3, 1, editor);
        if let Some(cell) = grid.cell_mut(1, 0) {
            cell.bg = Rgb::new(47, 52, 62);
        }
        grid.mark_default_colors(&[editor], &[]);
        let flagged = |grid: &CellGrid| {
            grid.row(0)
                .iter()
                .map(|cell| cell.attrs.contains(CellAttrs::DEFAULT_BACKGROUND))
                .collect::<Vec<_>>()
        };
        assert_eq!(flagged(&grid), vec![true, false, true]);

        if let Some(cell) = grid.cell_mut(0, 0) {
            cell.bg = Rgb::new(1, 2, 3);
        }
        grid.mark_default_colors(&[editor], &[]);
        assert_eq!(flagged(&grid), vec![false, false, true]);

        grid.mark_default_colors(&[editor, Rgb::new(47, 52, 62)], &[]);
        assert_eq!(flagged(&grid), vec![false, true, true]);
    }

    fn grid_of(rows: &[&str]) -> CellGrid {
        let cols = rows
            .iter()
            .map(|row| row.chars().count())
            .max()
            .unwrap_or(0);
        let mut grid = CellGrid::new(cols as u16, rows.len() as u16, Rgb::default());
        for (y, text) in rows.iter().enumerate() {
            for (x, ch) in text.chars().enumerate() {
                if let Some(cell) = grid.cell_mut(x as i32, y as i32) {
                    cell.glyph = ch.into();
                }
            }
        }
        grid
    }

    #[test]
    fn overlays_get_only_a_left_bar_in_the_column_before_their_origin() {
        let mut screen = grid_of(&["........", "........", "........", "........"]);
        let mut overlay = grid_of(&["ab", "cd"]);
        overlay.cursor = Some(CursorPosition { col: 1, row: 0 });
        overlay.cursor_shape = CursorShape::Underline;
        if let Some(cell) = overlay.cell_mut(0, 0) {
            cell.bg = Rgb::new(47, 52, 62);
        }
        let bar = Rgb::new(110, 110, 110);
        screen.overlay_with_left_bar(&overlay, CursorPosition { col: 2, row: 1 }, bar);
        assert_eq!(screen.row_text(0), "........");
        assert_eq!(screen.row_text(1), ".▌ab....");
        assert_eq!(screen.row_text(2), ".▌cd....");
        assert_eq!(screen.row_text(3), "........");
        for row in [1, 2] {
            let edge = screen.cell(1, row).copied().unwrap();
            assert_eq!((edge.fg, edge.bg), (bar, Rgb::new(47, 52, 62)));
        }
        assert_eq!(screen.cursor, Some(CursorPosition { col: 3, row: 1 }));
        assert_eq!(screen.cursor_shape, CursorShape::Underline);
    }

    #[test]
    fn overlay_bars_and_edges_split_wide_chars_they_cut() {
        let mut screen = CellGrid::new(5, 3, Rgb::default());
        for col in [0, 3] {
            if let Some(cell) = screen.cell_mut(col, 1) {
                cell.glyph = '漢'.into();
            }
            if let Some(cell) = screen.cell_mut(col + 1, 1) {
                cell.attrs = CellAttrs::WIDE_CONTINUATION;
            }
        }
        screen.overlay_with_left_bar(
            &grid_of(&["xy"]),
            CursorPosition { col: 2, row: 1 },
            Rgb::default(),
        );
        assert_eq!(screen.row_text(1), " ▌xy ");
    }

    #[test]
    fn clusters_keep_their_whole_text_past_sixty_five_thousand_entries() {
        for index in 0..70_000u32 {
            let mut cluster = String::from("a");
            for bit in 0..17 {
                cluster.push(if index & (1 << bit) == 0 {
                    '\u{301}'
                } else {
                    '\u{302}'
                });
            }
            let mut text = String::new();
            Glyph::from_cluster(&cluster).push_to(&mut text);
            assert_eq!(text, cluster);
        }
        for cluster in ["e\u{301}", "👩\u{200d}💻", "❤\u{fe0f}"] {
            let glyph = Glyph::from_cluster(cluster);
            let mut text = String::new();
            glyph.push_to(&mut text);
            assert_eq!(text, cluster);
            assert_eq!(Glyph::from_glyph_id(glyph.to_glyph_id()), Some(glyph));
        }
    }

    #[test]
    fn blending_respects_alpha() {
        let base = Rgb::new(0, 0, 0);
        assert_eq!(
            base.blend(gpui::opaque_grey(1., 0.5)),
            Rgb::new(128, 128, 128)
        );
    }
}
