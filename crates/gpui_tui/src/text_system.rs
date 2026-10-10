use std::borrow::Cow;

use anyhow::Result;
use gpui::{
    Bounds, DevicePixels, Font, FontId, FontMetrics, FontRun, FontStyle, FontWeight, GlyphId,
    LineLayout, Pixels, PlatformTextSystem, Point, RenderGlyphParams, ShapedGlyph, ShapedRun, Size,
    TextRenderingMode, point, px, size,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{CELL_WIDTH, grid::Glyph, size_for_cells};

const UNITS_PER_EM: u32 = 1000;
const ASCENT: f32 = 800.;
const DESCENT: f32 = -200.;
const NARROW_ADVANCE: f32 = 500.;

const BOLD_BIT: usize = 1;
const ITALIC_BIT: usize = 2;

#[derive(Default)]
pub struct TuiTextSystem;

pub fn is_bold(font_id: FontId) -> bool {
    font_id.0 & BOLD_BIT != 0
}

pub fn is_italic(font_id: FontId) -> bool {
    font_id.0 & ITALIC_BIT != 0
}

pub fn char_cells(ch: char) -> usize {
    ch.width().map_or(1, |width| width.min(2))
}

fn glyph_cells(glyph_id: GlyphId) -> usize {
    Glyph::from_glyph_id(glyph_id).map_or(1, Glyph::cells)
}

pub(crate) fn cluster_cells(cluster: &str) -> usize {
    UnicodeWidthStr::width(cluster).min(2)
}

impl PlatformTextSystem for TuiTextSystem {
    fn add_fonts(&self, _fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        Ok(())
    }

    fn all_font_names(&self) -> Vec<String> {
        vec!["Terminal".to_string()]
    }

    fn font_id(&self, descriptor: &Font) -> Result<FontId> {
        let mut id = 0;
        if descriptor.weight >= FontWeight::SEMIBOLD {
            id |= BOLD_BIT;
        }
        if descriptor.style != FontStyle::Normal {
            id |= ITALIC_BIT;
        }
        Ok(FontId(id))
    }

    fn font_metrics(&self, _font_id: FontId) -> FontMetrics {
        FontMetrics {
            units_per_em: UNITS_PER_EM,
            ascent: ASCENT,
            descent: DESCENT,
            line_gap: 0.,
            underline_position: -100.,
            underline_thickness: 50.,
            cap_height: 700.,
            x_height: 500.,
            bounding_box: Bounds {
                origin: point(0., DESCENT),
                size: size(NARROW_ADVANCE * 2., ASCENT - DESCENT),
            },
        }
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        Ok(Bounds {
            origin: point(0., DESCENT),
            size: size(self.advance(font_id, glyph_id)?.width, ASCENT - DESCENT),
        })
    }

    fn advance(&self, _font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        Ok(size(NARROW_ADVANCE * glyph_cells(glyph_id) as f32, 0.))
    }

    fn glyph_for_char(&self, _font_id: FontId, ch: char) -> Option<GlyphId> {
        Some(GlyphId(ch as u32))
    }

    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let font_size = params.font_size.as_f32() * params.scale_factor;
        let ascent = (font_size * ASCENT / UNITS_PER_EM as f32).round() as i32;
        let width = (CELL_WIDTH * glyph_cells(params.glyph_id) as f32 * params.scale_factor) as i32;
        Ok(Bounds {
            origin: Point::new(DevicePixels(0), DevicePixels(-ascent)),
            size: Size::new(
                DevicePixels(width.max(1)),
                DevicePixels((font_size.round() as i32).max(1)),
            ),
        })
    }

    fn rasterize_glyph(
        &self,
        _params: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        Ok((raster_bounds.size, Vec::new()))
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        let mut shaped_runs: Vec<ShapedRun> = Vec::new();
        let mut position = px(0.);
        let mut run_ends = runs.iter().scan(0, |end, run| {
            *end = (*end + run.len).min(text.len());
            Some((*end, run.font_id))
        });
        let mut run = run_ends.next();
        let mut glyphs = Vec::new();
        for (index, cluster) in text.grapheme_indices(true) {
            while let Some((run_end, font_id)) = run
                && index >= run_end
            {
                if !glyphs.is_empty() {
                    shaped_runs.push(ShapedRun {
                        font_id,
                        glyphs: std::mem::take(&mut glyphs),
                    });
                }
                run = run_ends.next();
            }
            if run.is_none() {
                break;
            }
            if cluster.starts_with(char::is_control) {
                position += px(CELL_WIDTH * cluster.chars().map(char_cells).sum::<usize>() as f32);
                continue;
            }
            let cells = cluster_cells(cluster);
            if cells > 0 {
                glyphs.push(ShapedGlyph {
                    id: Glyph::from_cluster(cluster).to_glyph_id(),
                    position: point(position, px(0.)),
                    index,
                    is_emoji: false,
                });
            }
            position += px(CELL_WIDTH * cells as f32);
        }
        if let Some((_, font_id)) = run
            && !glyphs.is_empty()
        {
            shaped_runs.push(ShapedRun { font_id, glyphs });
        }

        LineLayout {
            font_size,
            width: position,
            ascent: font_size * (ASCENT / UNITS_PER_EM as f32),
            descent: font_size * (-DESCENT / UNITS_PER_EM as f32),
            runs: shaped_runs,
            len: text.len(),
        }
    }

    fn cell_size(&self) -> Option<Size<Pixels>> {
        Some(size_for_cells(1, 1))
    }

    fn recommended_rendering_mode(
        &self,
        _font_id: FontId,
        _font_size: Pixels,
    ) -> TextRenderingMode {
        TextRenderingMode::Grayscale
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_ids_round_trip_codepoints() {
        let text_system = TuiTextSystem;
        for ch in ['a', 'Z', '한', '→', '🦀'] {
            let glyph = text_system.glyph_for_char(FontId(0), ch).unwrap();
            assert_eq!(char::from_u32(glyph.0), Some(ch));
        }
    }

    #[test]
    fn wide_chars_take_two_cells() {
        let text_system = TuiTextSystem;
        let text = "a한b";
        let layout = text_system.layout_line(
            text,
            px(12.),
            &[FontRun {
                len: text.len(),
                font_id: FontId(0),
            }],
        );
        let positions: Vec<f32> = layout.runs[0]
            .glyphs
            .iter()
            .map(|glyph| glyph.position.x.as_f32())
            .collect();
        assert_eq!(positions, vec![0., CELL_WIDTH, CELL_WIDTH * 3.]);
        assert_eq!(layout.width, px(CELL_WIDTH * 4.));
    }

    fn shaped(text: &str, runs: &[usize]) -> (Vec<(usize, String, f32, usize)>, Pixels) {
        let runs: Vec<FontRun> = runs
            .iter()
            .enumerate()
            .map(|(font, len)| FontRun {
                len: *len,
                font_id: FontId(font),
            })
            .collect();
        let layout = TuiTextSystem.layout_line(text, px(12.), &runs);
        let glyphs = layout
            .runs
            .iter()
            .flat_map(|run| {
                run.glyphs.iter().map(|glyph| {
                    let mut text = String::new();
                    Glyph::from_glyph_id(glyph.id).unwrap().push_to(&mut text);
                    (run.font_id.0, text, glyph.position.x.as_f32(), glyph.index)
                })
            })
            .collect();
        (glyphs, layout.width)
    }

    #[test]
    fn grapheme_clusters_are_one_glyph_holding_their_whole_text() {
        let text = "cafe\u{301}👩\u{200d}💻❤\u{fe0f}한x";
        let (glyphs, width) = shaped(text, &[text.len()]);
        let clusters: Vec<(&str, f32)> = glyphs
            .iter()
            .map(|(_, text, x, _)| (text.as_str(), *x / CELL_WIDTH))
            .collect();
        assert_eq!(
            clusters,
            [
                ("c", 0.),
                ("a", 1.),
                ("f", 2.),
                ("e\u{301}", 3.),
                ("👩\u{200d}💻", 4.),
                ("❤\u{fe0f}", 6.),
                ("한", 8.),
                ("x", 10.),
            ]
        );
        assert_eq!(width, px(CELL_WIDTH * 11.));
        let indices: Vec<usize> = glyphs.iter().map(|glyph| glyph.3).collect();
        assert_eq!(indices, [0, 1, 2, 3, 6, 17, 23, 26]);
    }

    #[test]
    fn style_runs_that_split_a_cluster_keep_it_whole() {
        let text = "ae\u{301}b";
        let (glyphs, _) = shaped(text, &[2, text.len() - 2]);
        let runs: Vec<(usize, &str)> = glyphs
            .iter()
            .map(|(font, text, _, _)| (*font, text.as_str()))
            .collect();
        assert_eq!(runs, [(0, "a"), (0, "e\u{301}"), (1, "b")]);
    }

    #[test]
    fn zero_width_text_alone_takes_no_cell() {
        let (glyphs, width) = shaped("\u{200b}x", &["\u{200b}x".len()]);
        assert_eq!(glyphs.len(), 1);
        assert_eq!(width, px(CELL_WIDTH));
    }

    #[test]
    fn wide_chars_paint_as_monochrome_glyphs_because_color_sprites_are_dropped() {
        let text_system = TuiTextSystem;
        let text = "한😀";
        let layout = text_system.layout_line(
            text,
            px(12.),
            &[FontRun {
                len: text.len(),
                font_id: FontId(0),
            }],
        );
        assert!(layout.runs[0].glyphs.iter().all(|glyph| !glyph.is_emoji));
    }

    #[test]
    fn bold_fonts_get_a_distinct_id() {
        let text_system = TuiTextSystem;
        let regular = text_system.font_id(&gpui::font("Zed Mono")).unwrap();
        let bold = text_system
            .font_id(&gpui::Font {
                weight: FontWeight::BOLD,
                ..gpui::font("Zed Mono")
            })
            .unwrap();
        assert!(!is_bold(regular));
        assert!(is_bold(bold));
    }

    #[test]
    fn em_widths_are_one_cell_at_every_font_size() {
        let text_system = gpui::TextSystem::new(std::sync::Arc::new(TuiTextSystem));
        let font_id = text_system.resolve_font(&gpui::font("Zed Mono"));
        for font_size in [px(10.), px(16.)] {
            assert_eq!(
                text_system.em_advance(font_id, font_size).unwrap(),
                px(CELL_WIDTH)
            );
            assert_eq!(
                text_system.em_width(font_id, font_size).unwrap(),
                px(CELL_WIDTH)
            );
        }
    }
}
