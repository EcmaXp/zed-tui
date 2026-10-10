use std::ops::Range;

use gpui::{
    AtlasKey, Bounds, ContentMask, Hsla, MonochromeSprite, Path, Point, PrimitiveBatch, Quad, Rgba,
    ScaledPixels, Scene, Underline,
};

use crate::{
    CELL_HEIGHT, CELL_WIDTH,
    atlas::TuiAtlas,
    device_cell_center,
    grid::{Cell, CellAttrs, CellGrid, Glyph, Rgb, UnderlineColor},
    text_system::{is_bold, is_italic},
};

const OPAQUE_ALPHA: f32 = 0.9;
const MIN_BOXED_ROWS: usize = 2;
const MIN_RULED_ROWS: usize = 3;
const MIN_LINE_CONTRAST: u32 = 12;
const CONTIGUOUS_EPSILON: f32 = 0.5;
const SAME_LINE_TOLERANCE: f32 = 3.;

fn to_bounds(bounds: &Bounds<ScaledPixels>) -> Bounds<f32> {
    bounds.map(|value| value.0)
}

fn clipped(bounds: &Bounds<ScaledPixels>, mask: &ContentMask<ScaledPixels>) -> Option<Bounds<f32>> {
    let clipped = to_bounds(bounds).intersect(&to_bounds(&mask.bounds));
    (!clipped.is_empty()).then_some(clipped)
}

fn covered_cols(bounds: &Bounds<f32>) -> Range<i32> {
    covered_cells(bounds.left(), bounds.right(), CELL_WIDTH)
}

fn covered_rows(bounds: &Bounds<f32>) -> Range<i32> {
    covered_cells(bounds.top(), bounds.bottom(), CELL_HEIGHT)
}

fn covered_cells(start: f32, end: f32, cell_size: f32) -> Range<i32> {
    let half = cell_size / 2.;
    let first = ((start - half) / cell_size).ceil() as i32;
    let last = ((end - half) / cell_size).ceil() as i32;
    first..last
}

fn intersect(a: Range<i32>, b: &Range<i32>) -> Range<i32> {
    a.start.max(b.start)..a.end.min(b.end)
}

pub(crate) fn canvas_if_untouched(background: Rgb, canvas: Rgb) -> Rgb {
    if background == Rgb::default() {
        canvas
    } else {
        background
    }
}

fn paint_line_glyph(cell: &mut Cell, ch: char, color: Rgba) -> bool {
    let fg = cell.bg.blend_rgba(color);
    if fg.distance(cell.bg) < MIN_LINE_CONTRAST {
        return false;
    }
    cell.glyph = ch.into();
    cell.fg = fg;
    cell.attrs = CellAttrs::empty();
    cell.underline = UnderlineColor::default();
    true
}

fn cell_of(x: f32, y: f32) -> (i32, i32) {
    (
        (x / CELL_WIDTH).floor() as i32,
        (y / CELL_HEIGHT).floor() as i32,
    )
}

#[derive(Default)]
struct RasterScratch {
    candidates: Vec<TextCandidate>,
    placements: Vec<Option<Placement>>,
    line_cursors: Vec<LineCursor>,
    rules_by_row: Vec<Vec<Range<i32>>>,
}

pub(crate) fn rasterize_scene(
    scene: &Scene,
    atlas: &TuiAtlas,
    icon_glyph: &dyn Fn(&str) -> Option<char>,
    cols: u16,
    rows: u16,
    canvas: Rgb,
) -> CellGrid {
    let scratch = &mut RasterScratch::default();
    layout_text(scene, atlas, icon_glyph, scratch);
    scratch.rules_by_row.resize_with(rows as usize, Vec::new);
    let mut rasterizer = Rasterizer {
        grid: CellGrid::new(cols, rows, Rgb::default()),
        canvas,
        scratch,
    };
    for batch in scene.batches() {
        match batch {
            PrimitiveBatch::Quads(range) => {
                for quad in scene.quads.get(range).unwrap_or(&[]) {
                    rasterizer.quad(quad);
                }
            }
            PrimitiveBatch::Paths(range) => {
                for path in scene.paths.get(range).unwrap_or(&[]) {
                    rasterizer.path(path);
                }
            }
            PrimitiveBatch::Underlines(range) => {
                for underline in scene.underlines.get(range).unwrap_or(&[]) {
                    rasterizer.underline(underline);
                }
            }
            PrimitiveBatch::MonochromeSprites { range, .. } => {
                for index in range {
                    rasterizer.sprite(index);
                }
            }
            PrimitiveBatch::Shadows(_)
            | PrimitiveBatch::SubpixelSprites { .. }
            | PrimitiveBatch::PolychromeSprites { .. }
            | PrimitiveBatch::Surfaces(_) => {}
        }
    }
    rasterizer.grid
}

type SpriteId = usize;

#[derive(Clone, Copy)]
struct Placement {
    col: i32,
    row: i32,
    glyph: Glyph,
    color: Rgba,
    attrs: CellAttrs,
}

struct LineCursor {
    center_y: f32,
    right: f32,
    next_col: i32,
}

struct TextCandidate {
    id: SpriteId,
    bounds: Bounds<f32>,
    center_y: f32,
    placement: Placement,
}

fn text_candidate(
    id: SpriteId,
    sprite: &MonochromeSprite,
    atlas: &TuiAtlas,
    icon_glyph: &dyn Fn(&str) -> Option<char>,
) -> Option<TextCandidate> {
    let key = atlas.key_for(sprite.tile.tile_id)?;
    let bounds = to_bounds(&sprite.bounds);
    let mask = to_bounds(&sprite.content_mask.bounds);
    let center = bounds.center();
    if !mask.contains(&center) {
        return None;
    }
    let (col, row, glyph, attrs) = match &key {
        AtlasKey::Glyph(params) => {
            let glyph = Glyph::from_glyph_id(params.glyph_id)?;
            let (col, row) = cell_of(bounds.left() + CELL_WIDTH / 2., center.y);
            let mut attrs = CellAttrs::empty();
            if is_bold(params.font_id) {
                attrs.insert(CellAttrs::BOLD);
            }
            if is_italic(params.font_id) {
                attrs.insert(CellAttrs::ITALIC);
            }
            (col, row, glyph, attrs)
        }
        AtlasKey::Svg(params) => {
            let glyph = Glyph::from_char(icon_glyph(&params.path)?);
            let (col, row) = cell_of(center.x, center.y);
            (col, row, glyph, CellAttrs::empty())
        }
        AtlasKey::Image(_) => return None,
    };
    let partly_hidden = bounds.top() < mask.top() || bounds.bottom() > mask.bottom();
    let cell_center_y = device_cell_center(col, row).y;
    if partly_hidden && !mask.contains(&Point::new(center.x, cell_center_y)) {
        return None;
    }
    let color = sprite.color.to_rgb();
    Some(TextCandidate {
        id,
        bounds,
        center_y: center.y,
        placement: Placement {
            col,
            row,
            glyph,
            color,
            attrs,
        },
    })
}

fn layout_text(
    scene: &Scene,
    atlas: &TuiAtlas,
    icon_glyph: &dyn Fn(&str) -> Option<char>,
    scratch: &mut RasterScratch,
) {
    let RasterScratch {
        candidates,
        placements,
        line_cursors,
        ..
    } = scratch;
    candidates.extend(
        scene
            .monochrome_sprites
            .iter()
            .enumerate()
            .filter_map(|(id, sprite)| text_candidate(id, sprite, atlas, icon_glyph)),
    );
    candidates.sort_unstable_by(|a, b| {
        a.placement
            .row
            .cmp(&b.placement.row)
            .then(a.bounds.left().total_cmp(&b.bounds.left()))
            .then(a.id.cmp(&b.id))
    });

    placements.resize(scene.monochrome_sprites.len(), None);
    let mut current_row = None;
    for candidate in candidates.iter() {
        let row = candidate.placement.row;
        if current_row != Some(row) {
            line_cursors.clear();
            current_row = Some(row);
        }
        let natural_col = candidate.placement.col;
        let line = line_cursors
            .iter_mut()
            .find(|line| (line.center_y - candidate.center_y).abs() <= SAME_LINE_TOLERANCE);
        let col = match &line {
            Some(line) if (candidate.bounds.left() - line.right).abs() < CONTIGUOUS_EPSILON => {
                line.next_col
            }
            Some(line) if candidate.bounds.left() > line.right && natural_col <= line.next_col => {
                line.next_col + 1
            }
            _ => natural_col,
        };
        let cursor = LineCursor {
            center_y: candidate.center_y,
            right: candidate.bounds.right(),
            next_col: col + candidate.placement.glyph.cells() as i32,
        };
        match line {
            Some(line) => *line = cursor,
            None => line_cursors.push(cursor),
        }
        if let Some(slot) = placements.get_mut(candidate.id) {
            *slot = Some(Placement {
                col,
                ..candidate.placement
            });
        }
    }
}

struct Rasterizer<'a> {
    grid: CellGrid,
    canvas: Rgb,
    scratch: &'a mut RasterScratch,
}

impl Rasterizer<'_> {
    fn quad(&mut self, quad: &Quad) {
        let Some(rect) = clipped(&quad.bounds, &quad.content_mask) else {
            return;
        };
        let background = quad.background.as_solid().filter(|color| color.a > 0.);

        if let Some(color) = background {
            let is_narrow = rect.size.width < CELL_WIDTH / 2.;
            let is_flat = rect.size.height < CELL_HEIGHT / 2.;
            if is_narrow && !is_flat {
                self.vertical_bar(&rect, color);
                return;
            }
            if is_flat && !is_narrow {
                self.horizontal_rule(&rect, color);
                return;
            }
            self.fill(&rect, color);
        }

        let border = quad.border_widths;
        if quad.border_color.a > 0. && border.any(|width| width.0 > 0.) {
            self.border(&to_bounds(&quad.bounds), &rect, quad);
        }
    }

    fn fill(&mut self, rect: &Bounds<f32>, color: Hsla) {
        let clears_text = color.a >= OPAQUE_ALPHA;
        let rgba = color.to_rgb();
        let cols = intersect(covered_cols(rect), &(0..self.grid.cols.into()));
        if cols.is_empty() {
            return;
        }
        let (first_col, end_col) = (cols.start, cols.end);
        let canvas = self.canvas;
        for row in intersect(covered_rows(rect), &(0..self.grid.rows.into())) {
            if clears_text {
                self.clear_char(first_col, row);
                self.clear_char(end_col - 1, row);
            }
            for col in cols.clone() {
                if let Some(cell) = self.grid.cell_mut(col, row) {
                    if clears_text {
                        cell.glyph = ' '.into();
                        cell.attrs = CellAttrs::empty();
                        cell.underline = UnderlineColor::default();
                    }
                    cell.bg = canvas_if_untouched(cell.bg, canvas).blend_rgba(rgba);
                }
            }
        }
    }

    fn vertical_bar(&mut self, rect: &Bounds<f32>, color: Hsla) {
        let rgba = color.to_rgb();
        let col = (rect.center().x / CELL_WIDTH).floor() as i32;
        for row in covered_rows(rect) {
            self.divider_char(col, row, rgba);
        }
    }

    fn divider_char(&mut self, col: i32, row: i32, color: Rgba) {
        if let Some(cell) = self.grid.cell_mut(col, row)
            && cell.glyph == '─'
        {
            paint_line_glyph(cell, '│', color);
            return;
        }
        self.line_char(col, row, '│', color);
    }

    fn horizontal_rule(&mut self, rect: &Bounds<f32>, color: Hsla) {
        let row = (rect.center().y / CELL_HEIGHT).floor() as i32;
        let rgba = color.to_rgb();
        let cols = covered_cols(rect);
        let mut drew = false;
        for col in cols.clone() {
            drew |= self.line_char(col, row, '─', rgba);
        }
        if drew && let Some(rules) = self.rules_in_row(row) {
            rules.push(cols);
        }
    }

    fn rules_in_row(&mut self, row: i32) -> Option<&mut Vec<Range<i32>>> {
        usize::try_from(row)
            .ok()
            .and_then(|row| self.scratch.rules_by_row.get_mut(row))
    }

    fn remove_rule_under_text(&mut self, col: i32, row: i32) {
        let Some(rules) = self.rules_in_row(row) else {
            return;
        };
        let Some(index) = rules.iter().position(|cols| cols.contains(&col)) else {
            return;
        };
        let cols = rules.swap_remove(index);
        for col in cols {
            if let Some(cell) = self.grid.cell_mut(col, row)
                && cell.glyph == '─'
            {
                cell.glyph = ' '.into();
            }
        }
    }

    fn is_blank_between(&self, row: i32, cols: Range<i32>) -> bool {
        cols.filter_map(|col| self.grid.cell(col, row))
            .all(|cell| cell.glyph == ' ')
    }

    fn line_char(&mut self, col: i32, row: i32, ch: char, color: Rgba) -> bool {
        self.grid
            .cell_mut(col, row)
            .filter(|cell| cell.glyph == ' ' && !cell.is_wide_continuation())
            .is_some_and(|cell| paint_line_glyph(cell, ch, color))
    }

    fn border(&mut self, full: &Bounds<f32>, clipped: &Bounds<f32>, quad: &Quad) {
        let cols = covered_cols(full);
        let rows = covered_rows(full);
        if cols.is_empty() || rows.is_empty() {
            return;
        }
        let (first_col, last_col) = (cols.start, cols.end - 1);
        let (first_row, last_row) = (rows.start, rows.end - 1);
        let edges = quad.border_widths;
        let color = quad.border_color.to_rgb();
        let is_boxed = [edges.top, edges.right, edges.bottom, edges.left]
            .iter()
            .all(|width| width.0 > 0.);
        let min_rows = if is_boxed {
            MIN_BOXED_ROWS
        } else {
            MIN_RULED_ROWS
        };
        let has_top = edges.top.0 > 0. && rows.len() >= min_rows;
        let has_bottom = edges.bottom.0 > 0. && rows.len() >= min_rows;
        let has_left = edges.left.0 > 0. && cols.len() > 1;
        let has_right = edges.right.0 > 0. && cols.len() > 1;
        let is_side_strip = clipped.size.width < CELL_WIDTH / 2.;

        for row in rows {
            let top = has_top && row == first_row;
            let bottom = has_bottom && row == last_row;
            let is_edge_row = top || bottom;
            let is_blank_frame_row =
                is_boxed && is_edge_row && self.is_blank_between(row, first_col + 1..last_col);
            let cell_top = row as f32 * CELL_HEIGHT;
            let cell_center_y = device_cell_center(first_col, row).y;
            let covers_center = clipped.top() <= cell_center_y && cell_center_y < clipped.bottom();
            let covers_frame_row = is_blank_frame_row
                && clipped.top() < cell_top + CELL_HEIGHT
                && clipped.bottom() > cell_top;
            let draws_row = covers_center || covers_frame_row;
            for col in cols.clone().filter(|_| draws_row) {
                let cell_left = col as f32 * CELL_WIDTH;
                let overlaps_column =
                    clipped.left() < cell_left + CELL_WIDTH && clipped.right() > cell_left;
                if !overlaps_column {
                    continue;
                }
                let left = has_left && col == first_col;
                let right = has_right && col == last_col;
                if is_side_strip && !left && !right {
                    continue;
                }
                let ch = match (top, bottom, left, right) {
                    (true, _, true, _) => '┌',
                    (true, _, _, true) => '┐',
                    (_, true, true, _) => '└',
                    (_, true, _, true) => '┘',
                    (true, _, _, _) | (_, true, _, _) => '─',
                    (_, _, true, _) | (_, _, _, true) => '│',
                    _ => continue,
                };
                self.line_char(col, row, ch, color);
            }
            if is_edge_row && let Some(rules) = self.rules_in_row(row) {
                rules.push(first_col + 1..last_col);
            }
        }
    }

    fn path(&mut self, path: &Path<ScaledPixels>) {
        let Some(color) = path.color.as_solid().filter(|color| color.a > 0.) else {
            return;
        };
        let Some(rect) = clipped(&path.bounds, &path.content_mask) else {
            return;
        };
        let rgba = color.to_rgb();
        let rows = intersect(covered_rows(&rect), &(0..self.grid.rows.into()));
        let cols = intersect(covered_cols(&rect), &(0..self.grid.cols.into()));
        for row in rows {
            for col in cols.clone() {
                let center = device_cell_center(col, row);
                let covered = path.vertices.chunks_exact(3).any(|triangle| {
                    let triangle = [0, 1, 2].map(|index| {
                        let position = triangle[index].xy_position;
                        (position.x.0, position.y.0)
                    });
                    triangle_contains(&triangle, (center.x, center.y))
                });
                if covered && let Some(cell) = self.grid.cell_mut(col, row) {
                    cell.bg = canvas_if_untouched(cell.bg, self.canvas).blend_rgba(rgba);
                }
            }
        }
    }

    fn underline(&mut self, underline: &Underline) {
        let Some(rect) = clipped(&underline.bounds, &underline.content_mask) else {
            return;
        };
        let row = ((rect.top() - 1.) / CELL_HEIGHT).floor() as i32;
        let style = if underline.wavy == true.into() {
            CellAttrs::UNDERLINE | CellAttrs::CURLY_UNDERLINE
        } else {
            CellAttrs::UNDERLINE
        };
        let color = underline.color.to_rgb();
        for col in covered_cols(&rect) {
            if let Some(cell) = self.grid.cell_mut(col, row) {
                cell.attrs.remove(CellAttrs::CURLY_UNDERLINE);
                cell.attrs.insert(style);
                cell.underline = UnderlineColor::of(cell.bg.blend_rgba(color));
            }
        }
    }

    fn sprite(&mut self, id: SpriteId) {
        if let Some(placement) = self.scratch.placements.get(id).copied().flatten() {
            self.put_char(
                placement.col,
                placement.row,
                placement.glyph,
                placement.color,
                placement.attrs,
            );
        }
    }

    fn clear_char(&mut self, col: i32, row: i32) {
        self.grid.split_wide_char_at(col, row);
        self.grid.split_wide_char_at(col + 1, row);
        if let Some(cell) = self.grid.cell_mut(col, row) {
            cell.glyph = ' '.into();
            cell.attrs = CellAttrs::empty();
            cell.underline = UnderlineColor::default();
        }
    }

    fn put_char(&mut self, col: i32, row: i32, glyph: Glyph, color: Rgba, attrs: CellAttrs) {
        self.remove_rule_under_text(col, row);
        let wide = glyph.cells() == 2;
        let Some(&Cell {
            attrs: shown_attrs,
            underline,
            ..
        }) = self.grid.cell(col, row)
        else {
            return;
        };
        if wide && self.grid.cell(col + 1, row).is_none() {
            return;
        }
        let kept = shown_attrs & (CellAttrs::UNDERLINE | CellAttrs::CURLY_UNDERLINE);
        self.clear_char(col, row);
        if wide {
            self.clear_char(col + 1, row);
        }
        if let Some(cell) = self.grid.cell_mut(col, row) {
            cell.glyph = glyph;
            cell.fg = cell.bg.blend_rgba(color);
            cell.attrs = attrs | kept;
            cell.underline = underline;
        }
        if wide && let Some(cell) = self.grid.cell_mut(col + 1, row) {
            cell.glyph = ' '.into();
            cell.attrs = CellAttrs::WIDE_CONTINUATION;
        }
    }
}

fn triangle_contains(triangle: &[(f32, f32); 3], point: (f32, f32)) -> bool {
    let sign = |a: (f32, f32), b: (f32, f32), c: (f32, f32)| {
        (a.0 - c.0) * (b.1 - c.1) - (b.0 - c.0) * (a.1 - c.1)
    };
    let d1 = sign(point, triangle[0], triangle[1]);
    let d2 = sign(point, triangle[1], triangle[2]);
    let d3 = sign(point, triangle[2], triangle[0]);
    let has_negative = d1 < 0. || d2 < 0. || d3 < 0.;
    let has_positive = d1 > 0. || d2 > 0. || d3 > 0.;
    !(has_negative && has_positive)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        AtlasTextureId, AtlasTextureKind, AtlasTile, DevicePixels, MonochromeSprite, Size, point,
        px, size,
    };

    fn rasterize(scene: &Scene, atlas: &TuiAtlas, cols: u16, rows: u16) -> CellGrid {
        rasterize_scene(scene, atlas, &chevron_icon, cols, rows, Rgb::default())
    }

    fn rasterize_on(scene: &Scene, canvas: Rgb, cols: u16, rows: u16) -> CellGrid {
        let atlas = TuiAtlas::default();
        rasterize_scene(scene, &atlas, &chevron_icon, cols, rows, canvas)
    }

    fn fill_quad(x: f32, y: f32, width: f32, height: f32, color: Hsla) -> Quad {
        Quad {
            bounds: scaled_bounds(x, y, width, height),
            content_mask: full_mask(),
            background: color.into(),
            ..Default::default()
        }
    }

    fn chevron_icon(path: &str) -> Option<char> {
        path.ends_with("chevron_right.svg").then_some('▸')
    }

    fn rgb(r: u8, g: u8, b: u8) -> Rgb {
        Rgb::new(r, g, b)
    }

    #[test]
    fn paths_cover_the_cells_whose_centers_lie_inside_a_triangle() {
        let atlas = TuiAtlas::default();
        let (cols, rows) = (24u16, 12u16);
        let mut seed = 0x2545_f491_u32;
        let mut next = |bound: f32| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed % 10_000) as f32 / 10_000. * bound
        };
        let mut covering_paths = 0;
        for _ in 0..50 {
            let width = cols as f32 * CELL_WIDTH;
            let height = rows as f32 * CELL_HEIGHT;
            let mut path = Path::new(point(px(next(width)), px(next(height))));
            for _ in 0..6 {
                path.line_to(point(
                    px(next(width + 40.) - 20.),
                    px(next(height + 40.) - 20.),
                ));
            }
            path.color = Hsla::white().opacity(0.5).into();
            let mut path = path.scale(1.);
            path.content_mask = ContentMask {
                bounds: scaled_bounds(next(40.), next(40.), next(width), next(height)),
            };

            let mut expected = CellGrid::new(cols, rows, Rgb::default());
            if let Some(rect) = clipped(&path.bounds, &path.content_mask) {
                let triangles: Vec<[(f32, f32); 3]> = path
                    .vertices
                    .chunks_exact(3)
                    .map(|triangle| {
                        [0, 1, 2].map(|index| {
                            let position = triangle[index].xy_position;
                            (position.x.0, position.y.0)
                        })
                    })
                    .collect();
                for row in covered_rows(&rect) {
                    for col in covered_cols(&rect) {
                        let center = device_cell_center(col, row);
                        if triangles
                            .iter()
                            .any(|triangle| triangle_contains(triangle, (center.x, center.y)))
                            && let Some(cell) = expected.cell_mut(col, row)
                        {
                            cell.bg = cell.bg.blend(Hsla::white().opacity(0.5));
                        }
                    }
                }
            }

            let mut scene = Scene::default();
            scene.insert_primitive(path);
            scene.finish();
            let grid = rasterize(&scene, &atlas, cols, rows);
            assert_eq!(grid, expected);
            covering_paths += (grid.cells.iter().any(|cell| cell.bg != Rgb::default())) as usize;
        }
        assert!(
            covering_paths > 25,
            "only {covering_paths} paths covered a cell"
        );
    }

    #[test]
    fn covered_cells_use_cell_centers() {
        assert_eq!(covered_cells(0., 16., 8.), 0..2);
        assert_eq!(covered_cells(3., 13., 8.), 0..2);
        assert_eq!(covered_cells(5., 11., 8.), 1..1);
        assert_eq!(covered_cells(5., 13., 8.), 1..2);
    }

    #[test]
    fn borders_matching_the_background_are_not_drawn() {
        let atlas = TuiAtlas::default();
        let mut scene = Scene::default();
        let background = gpui::opaque_grey(0.2, 1.);
        let border = |color: Hsla| Quad {
            bounds: scaled_bounds(0., 0., 32., 64.),
            content_mask: full_mask(),
            background: background.into(),
            border_color: color,
            border_widths: gpui::Edges::all(ScaledPixels(1.)),
            ..Default::default()
        };
        scene.insert_primitive(border(background));
        scene.finish();
        let grid = rasterize(&scene, &atlas, 4, 4);
        assert_eq!(grid.row_text(0), "    ");

        let mut scene = Scene::default();
        scene.insert_primitive(border(Hsla::white()));
        scene.finish();
        let grid = rasterize(&scene, &atlas, 4, 4);
        assert_eq!(grid.row_text(0), "┌──┐");
        assert_eq!(grid.row_text(1), "│  │");
        assert_eq!(grid.row_text(3), "└──┘");
    }

    fn underline(x: f32, wavy: bool, color: Hsla) -> Underline {
        Underline {
            order: 0,
            pad: 0,
            bounds: scaled_bounds(x, 14., 16., 1.),
            content_mask: full_mask(),
            color,
            thickness: ScaledPixels(1.),
            wavy: wavy.into(),
        }
    }

    #[test]
    fn underlines_keep_their_curl_and_color_under_text() {
        let atlas = TuiAtlas::default();
        let red = gpui::red();
        let mut scene = Scene::default();
        scene.insert_primitive(underline(0., true, red));
        scene.insert_primitive(underline(16., false, Hsla::white()));
        scene.insert_primitive(glyph_sprite(&atlas, 'x', 0.));
        scene.insert_primitive(glyph_sprite(&atlas, 'y', 16.));
        scene.finish();
        let grid = rasterize(&scene, &atlas, 4, 2);
        let curly = CellAttrs::UNDERLINE | CellAttrs::CURLY_UNDERLINE;
        let wavy = grid.cell(0, 0).unwrap();
        assert_eq!(wavy.glyph, 'x');
        assert!(wavy.attrs.contains(curly));
        assert_eq!(wavy.underline.rgb(), Some(wavy.bg.blend_rgba(red.to_rgb())));
        let straight = grid.cell(2, 0).unwrap();
        assert_eq!(straight.glyph, 'y');
        assert!(straight.attrs.contains(CellAttrs::UNDERLINE));
        assert!(!straight.attrs.contains(CellAttrs::CURLY_UNDERLINE));
        assert!(!grid.cell(0, 1).unwrap().attrs.intersects(curly));
    }

    #[test]
    fn translucent_fills_on_untouched_cells_blend_over_the_canvas() {
        let canvas = rgb(40, 44, 51);
        let selection = Hsla::from(gpui::rgba(0x74ade83d));
        let mut scene = Scene::default();
        scene.insert_primitive(fill_quad(0., 0., 16., 16., selection));
        scene.finish();
        let grid = rasterize_on(&scene, canvas, 3, 1);
        let selected = canvas.blend(selection);
        assert_eq!(
            grid.row(0).iter().map(|cell| cell.bg).collect::<Vec<_>>(),
            [selected, selected, Rgb::default()]
        );
    }

    fn canvas_fills(fills: &[(f32, Hsla)], canvas: Rgb) -> Vec<Rgb> {
        let mut scene = Scene::default();
        for (width, color) in fills {
            scene.insert_primitive(fill_quad(0., 0., *width, 16., *color));
        }
        scene.finish();
        let grid = rasterize_on(&scene, canvas, 3, 1);
        grid.row(0).iter().map(|cell| cell.bg).collect()
    }

    #[test]
    fn opaque_fills_in_the_canvas_color_stay_distinct_from_untouched_cells() {
        let canvas = rgb(40, 44, 51);
        let tab = Hsla::from(gpui::rgb(0x282c33));
        assert_eq!(
            canvas_fills(&[(8., tab)], canvas),
            [canvas, Rgb::default(), Rgb::default()]
        );
    }

    #[test]
    fn translucent_fills_stack_over_the_fill_below_not_the_canvas() {
        let canvas = rgb(40, 44, 51);
        let surface = Hsla::from(gpui::rgb(0x2f343e));
        let selection = Hsla::from(gpui::rgba(0x74ade83d));
        let surface_rgb = Rgb::default().blend(surface);
        assert_eq!(
            canvas_fills(&[(16., surface), (24., selection)], canvas),
            [
                surface_rgb.blend(selection),
                surface_rgb.blend(selection),
                canvas.blend(selection)
            ]
        );
    }

    fn bordered_quad(width: f32, height: f32) -> Scene {
        let mut scene = Scene::default();
        scene.insert_primitive(Quad {
            bounds: scaled_bounds(0., 0., width, height),
            content_mask: full_mask(),
            border_color: Hsla::white(),
            border_widths: gpui::Edges::all(ScaledPixels(1.)),
            ..Default::default()
        });
        scene.finish();
        scene
    }

    #[test]
    fn one_row_quads_only_get_side_borders() {
        let atlas = TuiAtlas::default();
        let grid = rasterize(&bordered_quad(32., 16.), &atlas, 4, 1);
        assert_eq!(grid.row_text(0), "│  │");
    }

    #[test]
    fn two_row_quads_get_a_full_frame() {
        let atlas = TuiAtlas::default();
        let grid = rasterize(&bordered_quad(32., 26.), &atlas, 4, 2);
        assert_eq!(grid.row_text(0), "┌──┐");
        assert_eq!(grid.row_text(1), "└──┘");
    }

    #[test]
    fn two_row_quads_with_one_rule_draw_nothing_across() {
        let atlas = TuiAtlas::default();
        let mut scene = Scene::default();
        scene.insert_primitive(Quad {
            bounds: scaled_bounds(0., 0., 32., 26.),
            content_mask: full_mask(),
            border_color: Hsla::white(),
            border_widths: gpui::Edges {
                bottom: ScaledPixels(1.),
                ..Default::default()
            },
            ..Default::default()
        });
        scene.finish();
        let grid = rasterize(&scene, &atlas, 4, 2);
        assert_eq!(grid.row_text(1), "    ");
    }

    #[test]
    fn vertical_lines_skip_the_second_half_of_wide_chars() {
        let atlas = TuiAtlas::default();
        let mut scene = Scene::default();
        let mut wide_glyph = glyph_sprite(&atlas, '한', 0.);
        wide_glyph.bounds.size.width = ScaledPixels(16.);
        scene.insert_primitive(wide_glyph);
        scene.insert_primitive(fill_quad(11., 0., 1., 32., Hsla::white()));
        scene.finish();
        let grid = rasterize(&scene, &atlas, 3, 2);
        assert_eq!(grid.row_text(0), "한 ");
        assert_eq!(grid.row_text(1), " │ ");
    }

    #[test]
    fn triangle_hit_test() {
        let triangle = [(0., 0.), (10., 0.), (0., 10.)];
        assert!(triangle_contains(&triangle, (2., 2.)));
        assert!(!triangle_contains(&triangle, (9., 9.)));
    }

    fn scaled_bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
        Bounds {
            origin: point(ScaledPixels(x), ScaledPixels(y)),
            size: size(ScaledPixels(width), ScaledPixels(height)),
        }
    }

    fn full_mask() -> ContentMask<ScaledPixels> {
        ContentMask {
            bounds: scaled_bounds(0., 0., 10_000., 10_000.),
        }
    }

    fn glyph_tile(atlas: &TuiAtlas, ch: char) -> AtlasTile {
        use gpui::PlatformAtlas as _;
        let params = gpui::RenderGlyphParams {
            font_id: gpui::FontId(0),
            glyph_id: gpui::GlyphId(ch as u32),
            font_size: gpui::px(16.),
            subpixel_variant: Default::default(),
            scale_factor: 1.,
            is_emoji: false,
            subpixel_rendering: false,
            dilation: 0,
        };
        atlas
            .get_or_insert_with(AtlasKey::Glyph(params), &mut || {
                Ok(Some((
                    Size::new(DevicePixels(8), DevicePixels(16)),
                    std::borrow::Cow::Borrowed(&[][..]),
                )))
            })
            .unwrap()
            .unwrap()
    }

    #[test]
    fn scene_with_quad_and_glyphs_renders_cells() {
        let atlas = TuiAtlas::default();
        let mut scene = Scene::default();
        scene.insert_primitive(fill_quad(0., 16., 32., 16., Hsla::red()));
        for (index, ch) in "hi".chars().enumerate() {
            let mut sprite = glyph_sprite(&atlas, ch, 8. + index as f32 * 8.);
            sprite.bounds.origin.y = ScaledPixels(19.);
            scene.insert_primitive(sprite);
        }
        scene.insert_primitive(fill_quad(8., 16., 2., 16., Hsla::white()));
        scene.finish();

        let grid = rasterize(&scene, &atlas, 6, 3);
        assert_eq!(grid.row_text(1), " hi   ");
        assert_eq!(grid.cell(0, 1).unwrap().bg, rgb(255, 0, 0));
        assert_eq!(grid.cell(5, 1).unwrap().bg, rgb(0, 0, 0));
        assert_eq!(grid.cell(1, 1).unwrap().fg, rgb(255, 255, 255));
    }

    #[test]
    fn vertical_dividers_stay_unbroken_where_a_horizontal_rule_crosses_them() {
        let atlas = TuiAtlas::default();
        let mut scene = Scene::default();
        scene.insert_primitive(fill_quad(0., 16., 40., 1., Hsla::white()));
        scene.insert_primitive(fill_quad(19., 0., 1., 48., Hsla::white()));
        scene.finish();
        let grid = rasterize(&scene, &atlas, 5, 3);
        assert_eq!(grid.row_text(0), "  │  ");
        assert_eq!(grid.row_text(1), "──│──");
        assert_eq!(grid.row_text(2), "  │  ");
    }

    fn glyph_sprite(atlas: &TuiAtlas, ch: char, x: f32) -> MonochromeSprite {
        MonochromeSprite {
            order: 0,
            pad: 0,
            bounds: scaled_bounds(x, 3., 8., 16.),
            content_mask: full_mask(),
            color: Hsla::white(),
            tile: glyph_tile(atlas, ch),
            transformation: gpui::TransformationMatrix::unit(),
        }
    }

    #[test]
    fn small_gaps_between_text_runs_keep_a_blank_cell() {
        let atlas = TuiAtlas::default();
        let mut scene = Scene::default();
        let name_start = 13.;
        let path_start = name_start + 16. + 2.5;
        let glyphs = [
            ('d', path_start + 8.),
            ('b', name_start + 8.),
            ('c', path_start),
            ('a', name_start),
        ];
        for (ch, x) in glyphs {
            scene.insert_primitive(glyph_sprite(&atlas, ch, x));
        }
        scene.finish();
        let grid = rasterize(&scene, &atlas, 8, 1);
        assert_eq!(grid.row_text(0), "  ab cd ");
    }

    #[test]
    fn icons_and_labels_in_different_layers_keep_a_gap() {
        use gpui::PlatformAtlas as _;
        let atlas = TuiAtlas::default();
        let icon_tile = atlas
            .get_or_insert_with(
                AtlasKey::Svg(gpui::RenderSvgParams {
                    path: "icons/chevron_right.svg".into(),
                    size: Size::new(DevicePixels(9), DevicePixels(9)),
                }),
                &mut || Ok(None),
            )
            .unwrap()
            .unwrap();
        let mut scene = Scene::default();
        scene.push_layer(scaled_bounds(0., 0., 64., 16.));
        scene.insert_primitive(glyph_sprite(&atlas, 'a', 19.5));
        scene.pop_layer();
        scene.insert_primitive(Quad {
            bounds: scaled_bounds(0., 0., 64., 16.),
            content_mask: full_mask(),
            ..Default::default()
        });
        scene.insert_primitive(MonochromeSprite {
            order: 0,
            pad: 0,
            bounds: scaled_bounds(8., 4., 9., 9.),
            content_mask: full_mask(),
            color: Hsla::white(),
            tile: icon_tile,
            transformation: gpui::TransformationMatrix::unit(),
        });
        scene.finish();
        let grid = rasterize(&scene, &atlas, 6, 1);
        assert_eq!(grid.row_text(0), " ▸ a  ");
    }

    #[test]
    fn contiguous_runs_stay_adjacent() {
        let atlas = TuiAtlas::default();
        let mut scene = Scene::default();
        for (index, ch) in "abcd".chars().enumerate() {
            scene.insert_primitive(glyph_sprite(&atlas, ch, 5. + index as f32 * 8.));
        }
        scene.finish();
        let grid = rasterize(&scene, &atlas, 6, 1);
        assert_eq!(grid.row_text(0), " abcd ");
    }

    #[test]
    fn box_edges_that_share_a_row_with_text_keep_only_their_corners() {
        let atlas = TuiAtlas::default();
        let mut scene = Scene::default();
        scene.insert_primitive(Quad {
            bounds: scaled_bounds(0., 0., 48., 32.),
            content_mask: full_mask(),
            border_color: Hsla::white(),
            border_widths: gpui::Edges::all(ScaledPixels(1.)),
            ..Default::default()
        });
        scene.push_layer(scaled_bounds(0., 0., 48., 32.));
        scene.insert_primitive(glyph_sprite(&atlas, 'a', 16.));
        scene.pop_layer();
        scene.finish();
        let grid = rasterize(&scene, &atlas, 6, 2);
        assert_eq!(grid.row_text(0), "┌ a  ┐");
        assert_eq!(grid.row_text(1), "└────┘");
    }

    #[test]
    fn rules_that_share_a_row_with_text_are_removed() {
        let atlas = TuiAtlas::default();
        let mut scene = Scene::default();
        scene.insert_primitive(fill_quad(0., 7., 48., 1., Hsla::white()));
        scene.push_layer(scaled_bounds(0., 0., 48., 16.));
        scene.insert_primitive(glyph_sprite(&atlas, 'a', 0.));
        scene.pop_layer();
        scene.finish();
        let grid = rasterize(&scene, &atlas, 6, 1);
        assert_eq!(grid.row_text(0), "a     ");

        let mut scene = Scene::default();
        scene.insert_primitive(fill_quad(0., 7., 48., 1., Hsla::white()));
        scene.finish();
        let grid = rasterize(&scene, &atlas, 6, 1);
        assert_eq!(grid.row_text(0), "──────");
    }

    #[test]
    fn content_mask_clips_glyphs() {
        let atlas = TuiAtlas::default();
        let mut scene = Scene::default();
        let mask = ContentMask {
            bounds: scaled_bounds(0., 0., 8., 16.),
        };
        for (index, ch) in "ab".chars().enumerate() {
            let mut sprite = glyph_sprite(&atlas, ch, index as f32 * 8.);
            sprite.content_mask = mask;
            scene.insert_primitive(sprite);
        }
        scene.finish();
        let grid = rasterize(&scene, &atlas, 4, 1);
        assert_eq!(grid.row_text(0), "a   ");
    }

    #[test]
    fn overlapping_lines_on_one_row_keep_their_own_columns() {
        let atlas = TuiAtlas::default();
        let mut scene = Scene::default();
        let mut scrolled = glyph_sprite(&atlas, 'x', 10.);
        scrolled.bounds.origin.y = ScaledPixels(-2.);
        scene.insert_primitive(scrolled);
        scene.push_layer(scaled_bounds(0., 0., 48., 16.));
        for (index, ch) in "zed".chars().enumerate() {
            scene.insert_primitive(glyph_sprite(&atlas, ch, 19. + index as f32 * 8.));
        }
        scene.pop_layer();
        scene.finish();
        let grid = rasterize(&scene, &atlas, 6, 1);
        assert_eq!(grid.row_text(0), " xzed ");
    }

    #[test]
    fn partly_hidden_glyphs_do_not_spill_into_the_row_above() {
        let atlas = TuiAtlas::default();
        let mut scene = Scene::default();
        let list = ContentMask {
            bounds: scaled_bounds(0., 12., 16., 40.),
        };
        for (ch, y) in [('a', 6.), ('b', 20.)] {
            let mut sprite = glyph_sprite(&atlas, ch, 0.);
            sprite.bounds.origin.y = ScaledPixels(y);
            sprite.content_mask = list;
            scene.insert_primitive(sprite);
        }
        scene.finish();
        let grid = rasterize(&scene, &atlas, 2, 2);
        assert_eq!(grid.row_text(0), "  ");
        assert_eq!(grid.row_text(1), "b ");
    }

    #[test]
    fn opaque_quads_hide_text_below() {
        let atlas = TuiAtlas::default();
        let mut scene = Scene::default();
        scene.insert_primitive(glyph_sprite(&atlas, 'x', 0.));
        scene.push_layer(scaled_bounds(0., 0., 16., 16.));
        scene.insert_primitive(fill_quad(0., 0., 16., 16., Hsla::black()));
        scene.pop_layer();
        scene.finish();
        let grid = rasterize(&scene, &atlas, 2, 1);
        assert_eq!(grid.row_text(0), "  ");
    }

    #[test]
    fn unused_texture_ids_are_ignored() {
        let atlas = TuiAtlas::default();
        let mut scene = Scene::default();
        scene.insert_primitive(MonochromeSprite {
            order: 0,
            pad: 0,
            bounds: scaled_bounds(0., 0., 8., 16.),
            content_mask: full_mask(),
            color: Hsla::white(),
            tile: AtlasTile {
                texture_id: AtlasTextureId {
                    index: 0,
                    kind: AtlasTextureKind::Monochrome,
                },
                tile_id: gpui::TileId(999),
                padding: 0,
                bounds: Default::default(),
            },
            transformation: gpui::TransformationMatrix::unit(),
        });
        scene.finish();
        let grid = rasterize(&scene, &atlas, 2, 1);
        assert_eq!(grid.row_text(0), "  ");
    }
}
