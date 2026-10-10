use super::{EXPECT_MESSAGE, LayoutId, NodeContext, NodeMeasureFn};
use crate::{
    AbsoluteLength, Bounds, DefiniteLength, Pixels, Point, Size, Style, Window, size,
    util::round_half_toward_zero,
};
use taffy::TaffyTree;

pub(super) struct CellSnapper {
    cell_size: Size<f32>,
}

impl CellSnapper {
    pub(super) fn new(cell_size: Size<f32>) -> Self {
        Self { cell_size }
    }

    pub(super) fn request_layout(
        &mut self,
        tree: &mut TaffyTree<NodeContext>,
        mut taffy_style: taffy::style::Style,
        style: &Style,
        children: &[LayoutId],
    ) -> LayoutId {
        snap_to_cells(&mut taffy_style, style, self.cell_size);
        if children.is_empty() {
            tree.new_leaf(taffy_style)
        } else {
            tree.new_with_children(taffy_style, LayoutId::to_taffy_slice(children))
        }
        .expect(EXPECT_MESSAGE)
        .into()
    }

    pub(super) fn request_measured_layout(
        &mut self,
        tree: &mut TaffyTree<NodeContext>,
        mut taffy_style: taffy::style::Style,
        style: &Style,
        measure: NodeMeasureFn,
    ) -> LayoutId {
        snap_to_cells(&mut taffy_style, style, self.cell_size);
        tree.new_leaf_with_context(taffy_style, NodeContext { measure })
            .expect(EXPECT_MESSAGE)
            .into()
    }

    pub(super) fn snap_measured_size(&self, measured: Size<f32>) -> Size<f32> {
        size(
            ceil_to_cell(measured.width, self.cell_size.width),
            round_extent_to_cells(measured.height, self.cell_size.height),
        )
    }

    pub(super) fn snap_bounds(
        &self,
        origin: Point<f32>,
        far: Point<f32>,
        layout_size: taffy::geometry::Size<f32>,
    ) -> Bounds<f32> {
        let (x, width) = snap_axis(origin.x..far.x, self.cell_size.width, layout_size.width);
        let (y, height) = snap_axis(origin.y..far.y, self.cell_size.height, layout_size.height);
        Bounds::new(Point { x, y }, size(width, height))
    }
}

fn snap_axis(edges: std::ops::Range<f32>, cell: f32, layout_extent: f32) -> (f32, f32) {
    let start = round_to_cell(edges.start, cell);
    let end = round_to_cell(edges.end, cell);
    let extent = if end > start {
        end - start
    } else {
        round_half_toward_zero(layout_extent.max(0.0))
    };
    (start, extent)
}

fn length_value(length: impl Into<taffy::style::Dimension>) -> Option<f32> {
    let raw = length.into().into_raw();
    (raw.tag() == taffy::style::CompactLength::LENGTH_TAG).then(|| raw.value())
}

fn snap_to_cells(taffy_style: &mut taffy::style::Style, style: &Style, cell_size: Size<f32>) {
    let zero = taffy::style::LengthPercentage::length(0.);
    if is_pixel_nudge(style.padding.left) {
        taffy_style.padding.left = zero;
    }
    if is_pixel_nudge(style.padding.right) {
        taffy_style.padding.right = zero;
    }
    if is_pixel_nudge(style.gap.width) {
        taffy_style.gap.width = zero;
    }
    snap_style_to_cells(taffy_style, cell_size);
}

fn is_pixel_nudge(length: DefiniteLength) -> bool {
    matches!(
        length,
        DefiniteLength::Absolute(AbsoluteLength::Pixels(pixels))
            if pixels > Pixels::ZERO && pixels <= Pixels(1.)
    )
}

fn snap_length<T>(value: T, snap: impl Fn(f32) -> f32) -> T
where
    T: Copy + Into<taffy::style::Dimension> + taffy::style_helpers::FromLength,
{
    length_value(value).map_or(value, |length| T::from_length(snap(length)))
}

fn snap_style_to_cells(style: &mut taffy::style::Style, cell: Size<f32>) {
    let snap_width = |value| round_width_to_cells(value, cell.width);
    let snap_height = |value| round_extent_to_cells(value, cell.height);
    let snap_x = |value| round_to_cell(value, cell.width);
    let snap_y = |value| round_to_cell(value, cell.height);
    let snap_spacing_x = |value| round_spacing_to_cells(value, cell.width);
    for size in [&mut style.size, &mut style.min_size, &mut style.max_size] {
        size.width = snap_length(size.width, snap_width);
        size.height = snap_length(size.height, snap_height);
    }
    style.flex_basis = if matches!(
        style.flex_direction,
        taffy::style::FlexDirection::Row | taffy::style::FlexDirection::RowReverse
    ) {
        snap_length(style.flex_basis, snap_width)
    } else {
        snap_length(style.flex_basis, snap_height)
    };
    snap_edges(&mut style.inset, snap_x, snap_y);
    snap_edges(&mut style.margin, snap_spacing_x, snap_y);
    snap_edges(&mut style.padding, snap_spacing_x, snap_y);
    snap_edges(&mut style.border, snap_spacing_x, snap_y);
    style.gap.width = snap_length(style.gap.width, snap_spacing_x);
    style.gap.height = snap_length(style.gap.height, snap_y);
}

fn snap_edges<T>(
    edges: &mut taffy::geometry::Rect<T>,
    snap_x: impl Fn(f32) -> f32 + Copy,
    snap_y: impl Fn(f32) -> f32 + Copy,
) where
    T: Copy + Into<taffy::style::Dimension> + taffy::style_helpers::FromLength,
{
    edges.left = snap_length(edges.left, snap_x);
    edges.right = snap_length(edges.right, snap_x);
    edges.top = snap_length(edges.top, snap_y);
    edges.bottom = snap_length(edges.bottom, snap_y);
}

impl Window {
    pub(crate) fn snap_to_cells(&self, point: Point<Pixels>) -> Point<Pixels> {
        let Some(cell_size) = self.text_system().cell_size() else {
            return point;
        };
        Point {
            x: Pixels(round_to_cell(point.x.0, cell_size.width.0)),
            y: Pixels(round_to_cell(point.y.0, cell_size.height.0)),
        }
    }

    pub(crate) fn cell_line_height(&self, line_height: Pixels) -> Pixels {
        self.text_system()
            .cell_size()
            .map_or(line_height, |cell_size| cell_size.height)
    }
}

fn round_to_cell(value: f32, cell: f32) -> f32 {
    round_half_toward_zero(value / cell) * cell
}

fn round_extent_to_cells(value: f32, cell: f32) -> f32 {
    if value < cell / 2.0 {
        value
    } else {
        (value / cell).round() * cell
    }
}

fn round_spacing_to_cells(value: f32, cell: f32) -> f32 {
    if value > 0.0 && value < cell {
        cell
    } else {
        round_to_cell(value, cell)
    }
}

fn round_width_to_cells(value: f32, cell: f32) -> f32 {
    if value <= 0.0 {
        value
    } else {
        (value / cell).floor().max(1.0) * cell
    }
}

fn ceil_to_cell(value: f32, cell: f32) -> f32 {
    (value / cell).ceil() * cell
}

#[cfg(test)]
mod tests {
    use super::*;
    use taffy::geometry::{Rect as TaffyRect, Size as TaffySize};

    #[test]
    fn cells_round_toward_zero_and_hairlines_keep_their_extent() {
        assert_eq!(round_to_cell(8., 16.), 0.);
        assert_eq!(round_to_cell(9., 16.), 16.);
        assert_eq!(round_to_cell(24., 16.), 16.);
        assert_eq!(round_to_cell(25., 16.), 32.);
        assert_eq!(round_to_cell(-1., 16.), 0.);
        assert_eq!(round_to_cell(-20., 16.), -16.);
        assert_eq!(round_to_cell(4., 8.), 0.);
        assert_eq!(round_to_cell(5., 8.), 8.);
        assert_eq!(round_extent_to_cells(1., 16.), 1.);
        assert_eq!(round_extent_to_cells(7.9, 16.), 7.9);
        assert_eq!(round_extent_to_cells(8., 16.), 16.);
        assert_eq!(round_extent_to_cells(20., 16.), 16.);
        assert_eq!(round_extent_to_cells(24., 16.), 32.);
        assert_eq!(round_extent_to_cells(8.75, 8.), 8.);
        assert_eq!(round_spacing_to_cells(0.5, 8.), 8.);
        assert_eq!(round_spacing_to_cells(13., 8.), 16.);
        assert_eq!(round_spacing_to_cells(0., 8.), 0.);
        assert_eq!(round_spacing_to_cells(-1., 8.), 0.);
        assert_eq!(round_width_to_cells(14., 8.), 8.);
        assert_eq!(round_width_to_cells(16., 8.), 16.);
        assert_eq!(round_width_to_cells(23., 8.), 16.);
        assert_eq!(round_width_to_cells(4., 8.), 8.);
        assert_eq!(round_width_to_cells(1., 8.), 8.);
        assert_eq!(round_width_to_cells(0., 8.), 0.);
        assert_eq!(ceil_to_cell(17., 8.), 24.);
        assert_eq!(ceil_to_cell(24., 8.), 24.);
    }

    #[test]
    fn cell_snapping_rounds_lengths_on_both_axes() {
        use taffy::style::{Dimension, LengthPercentage, LengthPercentageAuto};
        let length = LengthPercentage::length;
        let mut style = taffy::style::Style {
            size: TaffySize {
                width: Dimension::length(13.),
                height: Dimension::length(22.),
            },
            min_size: TaffySize {
                width: Dimension::auto(),
                height: Dimension::length(1.),
            },
            max_size: TaffySize {
                width: Dimension::auto(),
                height: Dimension::percent(0.5),
            },
            margin: TaffyRect {
                left: LengthPercentageAuto::length(3.),
                right: LengthPercentageAuto::length(3.),
                top: LengthPercentageAuto::length(-20.),
                bottom: LengthPercentageAuto::auto(),
            },
            padding: TaffyRect {
                left: length(5.),
                right: length(5.),
                top: length(3.),
                bottom: length(12.),
            },
            border: TaffyRect {
                left: length(1.),
                right: length(1.),
                top: length(1.),
                bottom: length(1.),
            },
            gap: TaffySize {
                width: length(4.),
                height: length(12.),
            },
            ..Default::default()
        };
        snap_style_to_cells(&mut style, size(8., 16.));

        assert_eq!(style.size.width, Dimension::length(8.));
        assert_eq!(style.size.height, Dimension::length(16.));
        assert_eq!(style.min_size.width, Dimension::auto());
        assert_eq!(style.min_size.height, Dimension::length(1.));
        assert_eq!(style.max_size.height, Dimension::percent(0.5));
        assert_eq!(style.margin.top, LengthPercentageAuto::length(-16.));
        assert_eq!(style.margin.bottom, LengthPercentageAuto::auto());
        assert_eq!(style.margin.left, LengthPercentageAuto::length(8.));
        assert_eq!(style.padding.top, length(0.));
        assert_eq!(style.padding.bottom, length(16.));
        assert_eq!(style.padding.left, length(8.));
        assert_eq!(style.border.top, length(0.));
        assert_eq!(style.border.left, length(8.));
        assert_eq!(style.gap.width, length(8.));
        assert_eq!(style.gap.height, length(16.));
    }
}
