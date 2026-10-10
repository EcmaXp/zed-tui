use std::ops::Range;

use gpui::{
    AtlasKey, Bounds, ContentMask, Hsla, MonochromeSprite, Point, PrimitiveBatch, Quad, Rgba,
    ScaledPixels, Scene, Underline,
};

use crate::{
    CELL_HEIGHT, CELL_WIDTH,
    atlas::TuiAtlas,
    device_cell_center,
    grid::{Cell, CellAttrs, CellGrid, Glyph, Rgb},
    text_system::{is_bold, is_italic},
};

const OPAQUE_ALPHA: f32 = 0.9;
const MIN_BOXED_ROWS: usize = 2;
const MIN_RULED_ROWS: usize = 3;
const MIN_LINE_CONTRAST: u32 = 12;

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

fn paint_line_glyph(cell: &mut Cell, ch: char, color: Rgba) -> bool {
    let fg = cell.bg.blend_rgba(color);
    if fg.distance(cell.bg) < MIN_LINE_CONTRAST {
        return false;
    }
    cell.glyph = ch.into();
    cell.fg = fg;
    cell.attrs = CellAttrs::empty();
    true
}

fn cell_of(x: f32, y: f32) -> (i32, i32) {
    (
        (x / CELL_WIDTH).floor() as i32,
        (y / CELL_HEIGHT).floor() as i32,
    )
}

pub(crate) fn rasterize_scene(scene: &Scene, atlas: &TuiAtlas, cols: u16, rows: u16) -> CellGrid {
    let mut rasterizer = Rasterizer {
        grid: CellGrid::new(cols, rows, Rgb::default()),
    };
    for batch in scene.batches() {
        match batch {
            PrimitiveBatch::Quads(range) => {
                for quad in scene.quads.get(range).unwrap_or(&[]) {
                    rasterizer.quad(quad);
                }
            }
            PrimitiveBatch::Underlines(range) => {
                for underline in scene.underlines.get(range).unwrap_or(&[]) {
                    rasterizer.underline(underline);
                }
            }
            PrimitiveBatch::MonochromeSprites { range, .. } => {
                for sprite in scene.monochrome_sprites.get(range).unwrap_or(&[]) {
                    rasterizer.sprite(sprite, atlas);
                }
            }
            PrimitiveBatch::Shadows(_)
            | PrimitiveBatch::Paths(_)
            | PrimitiveBatch::SubpixelSprites { .. }
            | PrimitiveBatch::PolychromeSprites { .. }
            | PrimitiveBatch::Surfaces(_) => {}
        }
    }
    rasterizer.grid
}

#[derive(Clone, Copy)]
struct Placement {
    col: i32,
    row: i32,
    glyph: Glyph,
    color: Rgba,
    attrs: CellAttrs,
}

fn placement(sprite: &MonochromeSprite, atlas: &TuiAtlas) -> Option<Placement> {
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
        AtlasKey::Svg(_) | AtlasKey::Image(_) => return None,
    };
    let partly_hidden = bounds.top() < mask.top() || bounds.bottom() > mask.bottom();
    let cell_center_y = device_cell_center(col, row).y;
    if partly_hidden && !mask.contains(&Point::new(center.x, cell_center_y)) {
        return None;
    }
    let color = sprite.color.to_rgb();
    Some(Placement {
        col,
        row,
        glyph,
        color,
        attrs,
    })
}

struct Rasterizer {
    grid: CellGrid,
}

impl Rasterizer {
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
                    }
                    cell.bg = cell.bg.blend_rgba(rgba);
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
        for col in covered_cols(rect) {
            self.line_char(col, row, '─', rgba);
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
        }
    }

    fn underline(&mut self, underline: &Underline) {
        let Some(rect) = clipped(&underline.bounds, &underline.content_mask) else {
            return;
        };
        let row = ((rect.top() - 1.) / CELL_HEIGHT).floor() as i32;
        for col in covered_cols(&rect) {
            if let Some(cell) = self.grid.cell_mut(col, row) {
                cell.attrs.insert(CellAttrs::UNDERLINE);
            }
        }
    }

    fn sprite(&mut self, sprite: &MonochromeSprite, atlas: &TuiAtlas) {
        if let Some(placement) = placement(sprite, atlas) {
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
        }
    }

    fn put_char(&mut self, col: i32, row: i32, glyph: Glyph, color: Rgba, attrs: CellAttrs) {
        let wide = glyph.cells() == 2;
        let Some(&Cell {
            attrs: shown_attrs, ..
        }) = self.grid.cell(col, row)
        else {
            return;
        };
        if wide && self.grid.cell(col + 1, row).is_none() {
            return;
        }
        let kept = shown_attrs & CellAttrs::UNDERLINE;
        self.clear_char(col, row);
        if wide {
            self.clear_char(col + 1, row);
        }
        if let Some(cell) = self.grid.cell_mut(col, row) {
            cell.glyph = glyph;
            cell.fg = cell.bg.blend_rgba(color);
            cell.attrs = attrs | kept;
        }
        if wide && let Some(cell) = self.grid.cell_mut(col + 1, row) {
            cell.glyph = ' '.into();
            cell.attrs = CellAttrs::WIDE_CONTINUATION;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        AtlasTextureId, AtlasTextureKind, AtlasTile, DevicePixels, MonochromeSprite, Size, point,
        size,
    };

    fn rasterize(scene: &Scene, atlas: &TuiAtlas, cols: u16, rows: u16) -> CellGrid {
        rasterize_scene(scene, atlas, cols, rows)
    }

    fn fill_quad(x: f32, y: f32, width: f32, height: f32, color: Hsla) -> Quad {
        Quad {
            bounds: scaled_bounds(x, y, width, height),
            content_mask: full_mask(),
            background: color.into(),
            ..Default::default()
        }
    }

    fn rgb(r: u8, g: u8, b: u8) -> Rgb {
        Rgb::new(r, g, b)
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
