use super::{EXPECT_MESSAGE, LayoutId, NodeContext, NodeMeasureFn};
use crate::{
    AbsoluteLength, Bounds, DefiniteLength, Length, Pixels, Point, Size, Style, Window, size,
    util::round_half_toward_zero,
};
use taffy::{TaffyTree, TraversePartialTree as _};

pub(super) struct CellSnapper {
    cell_size: Size<f32>,
    viewport_width: f32,
}

impl CellSnapper {
    pub(super) fn new(cell_size: Size<f32>) -> Self {
        Self {
            cell_size,
            viewport_width: f32::INFINITY,
        }
    }

    pub(super) fn set_viewport_width(&mut self, viewport_width: f32) {
        self.viewport_width = viewport_width;
    }

    pub(super) fn request_layout(
        &mut self,
        tree: &mut TaffyTree<NodeContext>,
        mut taffy_style: taffy::style::Style,
        style: &Style,
        children: &[LayoutId],
    ) -> LayoutId {
        snap_to_cells(&mut taffy_style, style, self.cell_size, self.viewport_width);
        if let Some(min_width) = width_around_fixed_children(tree, &taffy_style, children) {
            let min_width = if min_width > self.viewport_width {
                let edges =
                    horizontal_edges(&taffy_style.border) + horizontal_edges(&taffy_style.padding);
                narrow_fixed_children(
                    tree,
                    children.iter().map(|child| (*child).into()),
                    self.viewport_width - edges,
                    MAX_WRAPPER_DEPTH,
                );
                self.viewport_width
            } else {
                min_width
            };
            taffy_style.min_size.width = taffy::style::Dimension::length(min_width);
        }
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
        snap_to_cells(&mut taffy_style, style, self.cell_size, self.viewport_width);
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

fn is_row(style: &taffy::style::Style) -> bool {
    style.display == taffy::style::Display::Flex
        && matches!(
            style.flex_direction,
            taffy::style::FlexDirection::Row | taffy::style::FlexDirection::RowReverse
        )
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

const MAX_WRAPPER_DEPTH: usize = 3;

fn width_around_fixed_children(
    tree: &TaffyTree<NodeContext>,
    style: &taffy::style::Style,
    children: &[LayoutId],
) -> Option<f32> {
    let border = horizontal_edges(&style.border);
    if border <= 0.
        || !style.min_size.width.is_auto()
        || style.overflow.x == taffy::style::Overflow::Scroll
    {
        return None;
    }
    let widest = widest_fixed_width(
        tree,
        style,
        children.iter().map(|child| (*child).into()),
        MAX_WRAPPER_DEPTH,
    )?;
    let fitted = widest + border + horizontal_edges(&style.padding);
    match positive_length(style.size.width) {
        Some(width) if widest > width || fitted <= width => None,
        _ => Some(fitted),
    }
}

fn widest_fixed_width(
    tree: &TaffyTree<NodeContext>,
    style: &taffy::style::Style,
    children: impl Iterator<Item = taffy::NodeId>,
    depth: usize,
) -> Option<f32> {
    if is_row(style) {
        return None;
    }
    children
        .filter_map(|child| {
            let child_style = tree.style(child).ok()?;
            if child_style.position == taffy::style::Position::Absolute {
                return None;
            }
            if let Some(width) = positive_length(child_style.size.width) {
                return Some(width);
            }
            let widest = widest_fixed_width(
                tree,
                child_style,
                tree.child_ids(child),
                depth.checked_sub(1)?,
            )?;
            Some(
                widest
                    + horizontal_edges(&child_style.border)
                    + horizontal_edges(&child_style.padding),
            )
        })
        .reduce(f32::max)
}

fn narrow_fixed_children(
    tree: &mut TaffyTree<NodeContext>,
    children: impl Iterator<Item = taffy::NodeId>,
    limit: f32,
    depth: usize,
) {
    for child in children {
        let mut child_style = tree.style(child).expect(EXPECT_MESSAGE).clone();
        if child_style.position == taffy::style::Position::Absolute {
            continue;
        }
        if let Some(width) = positive_length(child_style.size.width) {
            if width > limit {
                child_style.size.width = taffy::style::Dimension::length(limit);
                tree.set_style(child, child_style).expect(EXPECT_MESSAGE);
            }
            continue;
        }
        let Some(depth) = depth.checked_sub(1) else {
            continue;
        };
        let edges = horizontal_edges(&child_style.border) + horizontal_edges(&child_style.padding);
        let grandchildren = tree.children(child).expect(EXPECT_MESSAGE);
        narrow_fixed_children(tree, grandchildren.into_iter(), limit - edges, depth);
    }
}

fn horizontal_edges(edges: &taffy::geometry::Rect<taffy::style::LengthPercentage>) -> f32 {
    let length = |value: taffy::style::LengthPercentage| positive_length(value).unwrap_or(0.);
    length(edges.left) + length(edges.right)
}

fn length_value(length: impl Into<taffy::style::Dimension>) -> Option<f32> {
    let raw = length.into().into_raw();
    (raw.tag() == taffy::style::CompactLength::LENGTH_TAG).then(|| raw.value())
}

fn positive_length(length: impl Into<taffy::style::Dimension>) -> Option<f32> {
    length_value(length).filter(|value| *value > 0.)
}

fn snap_to_cells(
    taffy_style: &mut taffy::style::Style,
    style: &Style,
    cell_size: Size<f32>,
    viewport_width: f32,
) {
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
    widen_text_widths(taffy_style, style, cell_size.width, viewport_width);
    snap_style_to_cells(taffy_style, cell_size);
}

const TEXT_REM_CELLS: f32 = 2.25;
const MIN_TEXT_WIDTH_REMS: f32 = 8.;

fn widen_text_widths(
    taffy_style: &mut taffy::style::Style,
    style: &Style,
    cell_width: f32,
    viewport_width: f32,
) {
    let text_width = |length: Length| match length {
        Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Rems(rems)))
            if rems.0 >= MIN_TEXT_WIDTH_REMS =>
        {
            Some((rems.0 * TEXT_REM_CELLS * cell_width).min(viewport_width))
        }
        _ => None,
    };
    if let Some(width) = text_width(style.max_size.width) {
        taffy_style.max_size.width =
            snap_length(taffy_style.max_size.width, |value| value.max(width));
    }
    if let Some(width) = text_width(style.size.width) {
        let widened = snap_length(taffy_style.size.width, |value| value.max(width));
        if widened != taffy_style.size.width && taffy_style.max_size.width.is_auto() {
            taffy_style.max_size.width = taffy::style::Dimension::percent(1.);
        }
        taffy_style.size.width = widened;
    }
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

    #[expect(missing_docs)]
    pub fn text_width_scale(&self) -> f32 {
        match self.text_system().cell_size() {
            Some(cell) => (TEXT_REM_CELLS * cell.width / self.rem_size()).max(1.),
            None => 1.,
        }
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

    fn width_style(width: AbsoluteLength) -> (Style, taffy::style::Style) {
        let mut style = Style::default();
        style.size.width = Length::Definite(DefiniteLength::Absolute(width));
        let pixels = width.to_pixels(Pixels(10.)).0;
        let taffy_style = taffy::style::Style {
            size: TaffySize {
                width: taffy::style::Dimension::length(pixels),
                height: taffy::style::Dimension::auto(),
            },
            ..Default::default()
        };
        (style, taffy_style)
    }

    #[test]
    fn rem_widths_for_text_widen_but_stay_within_the_parent() {
        let (style, mut taffy_style) = width_style(AbsoluteLength::Rems(crate::Rems(40.)));
        widen_text_widths(&mut taffy_style, &style, 8., f32::INFINITY);
        assert_eq!(
            taffy_style.size.width,
            taffy::style::Dimension::length(720.)
        );
        assert_eq!(
            taffy_style.max_size.width,
            taffy::style::Dimension::percent(1.)
        );
    }

    #[test]
    fn notification_bodies_fit_their_action_buttons() {
        let cells = |length: taffy::style::Dimension| length.into_raw().value() / 8.;
        let (toast_style, mut toast) = width_style(AbsoluteLength::Rems(crate::Rems(28.)));
        widen_text_widths(&mut toast, &toast_style, 8., f32::INFINITY);
        assert!(cells(toast.size.width) >= 57.);

        let mut body_style = Style::default();
        body_style.max_size.width = Length::Definite(DefiniteLength::Absolute(
            AbsoluteLength::Rems(crate::Rems(24.)),
        ));
        let mut body = taffy::style::Style {
            max_size: TaffySize {
                width: taffy::style::Dimension::length(240.),
                height: taffy::style::Dimension::auto(),
            },
            ..Default::default()
        };
        widen_text_widths(&mut body, &body_style, 8., f32::INFINITY);
        assert!(cells(body.max_size.width) >= 49.);
    }

    #[test]
    fn widened_widths_stop_at_the_viewport_but_never_shrink() {
        let (style, mut taffy_style) = width_style(AbsoluteLength::Rems(crate::Rems(40.)));
        widen_text_widths(&mut taffy_style, &style, 8., 640.);
        assert_eq!(
            taffy_style.size.width,
            taffy::style::Dimension::length(640.)
        );

        let (style, mut taffy_style) = width_style(AbsoluteLength::Rems(crate::Rems(40.)));
        let unchanged = taffy_style.clone();
        widen_text_widths(&mut taffy_style, &style, 8., 320.);
        assert_eq!(taffy_style, unchanged);
    }

    #[test]
    fn rem_widths_that_already_fit_their_text_keep_their_size() {
        let mut style = Style::default();
        style.size.width = Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Rems(
            crate::Rems(34.),
        )));
        let (_, mut taffy_style) = width_style(AbsoluteLength::Pixels(Pixels(34. * 20.)));
        let unchanged = taffy_style.clone();
        widen_text_widths(&mut taffy_style, &style, 8., f32::INFINITY);
        assert_eq!(taffy_style, unchanged);
    }

    #[test]
    fn rem_minimum_widths_reserve_layout_space_and_keep_their_size() {
        let mut style = Style::default();
        style.min_size.width = Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Rems(
            crate::Rems(16.),
        )));
        let mut taffy_style = taffy::style::Style {
            min_size: TaffySize {
                width: taffy::style::Dimension::length(160.),
                height: taffy::style::Dimension::auto(),
            },
            ..Default::default()
        };
        let unchanged = taffy_style.clone();
        widen_text_widths(&mut taffy_style, &style, 8., f32::INFINITY);
        assert_eq!(taffy_style, unchanged);
    }

    #[test]
    fn pixel_and_icon_widths_keep_their_size() {
        for width in [
            AbsoluteLength::Pixels(Pixels(400.)),
            AbsoluteLength::Rems(crate::Rems(1.25)),
        ] {
            let (style, mut taffy_style) = width_style(width);
            let unchanged = taffy_style.clone();
            widen_text_widths(&mut taffy_style, &style, 8., f32::INFINITY);
            assert_eq!(taffy_style, unchanged);
        }
    }

    #[test]
    fn bordered_wrappers_fit_their_fixed_width_children() {
        use taffy::style::{Dimension, LengthPercentage};
        let mut tree = TaffyTree::<NodeContext>::new();
        let child = tree
            .new_leaf(taffy::style::Style {
                size: TaffySize {
                    width: Dimension::length(400.),
                    height: Dimension::auto(),
                },
                ..Default::default()
            })
            .unwrap();
        let mut wrapper = taffy::style::Style {
            flex_direction: taffy::style::FlexDirection::Column,
            ..Default::default()
        };
        wrapper.border.left = LengthPercentage::length(8.);
        wrapper.border.right = LengthPercentage::length(8.);
        let children = [LayoutId::from(child)];
        assert_eq!(
            width_around_fixed_children(&tree, &wrapper, &children),
            Some(416.)
        );
        let aside = tree
            .new_leaf(taffy::style::Style {
                position: taffy::style::Position::Absolute,
                size: TaffySize {
                    width: Dimension::length(900.),
                    height: Dimension::auto(),
                },
                ..Default::default()
            })
            .unwrap();
        let auto_wrapper = tree
            .new_with_children(
                taffy::style::Style {
                    display: taffy::style::Display::Block,
                    ..Default::default()
                },
                &[child, aside],
            )
            .unwrap();
        assert_eq!(
            width_around_fixed_children(&tree, &wrapper, &[LayoutId::from(auto_wrapper)]),
            Some(416.)
        );
        for (width, expected) in [(400., Some(416.)), (480., None), (300., None)] {
            wrapper.size.width = Dimension::length(width);
            assert_eq!(
                width_around_fixed_children(&tree, &wrapper, &children),
                expected,
                "box width {width}"
            );
        }
        wrapper.border.left = LengthPercentage::length(0.);
        wrapper.border.right = LengthPercentage::length(0.);
        assert_eq!(
            width_around_fixed_children(&tree, &wrapper, &children),
            None
        );
    }

    #[test]
    fn bordered_wrappers_stay_within_the_viewport_by_narrowing_their_children() {
        use taffy::style::{Dimension, LengthPercentage};
        let fixed = |width| taffy::style::Style {
            size: TaffySize {
                width: Dimension::length(width),
                height: Dimension::auto(),
            },
            ..Default::default()
        };
        let mut tree = TaffyTree::<NodeContext>::new();
        let results = tree.new_leaf(fixed(640.)).unwrap();
        let footer = tree.new_leaf(fixed(320.)).unwrap();
        let column = tree
            .new_with_children(
                taffy::style::Style {
                    display: taffy::style::Display::Block,
                    ..Default::default()
                },
                &[results],
            )
            .unwrap();
        let mut frame = taffy::style::Style {
            flex_direction: taffy::style::FlexDirection::Column,
            ..Default::default()
        };
        frame.border.left = LengthPercentage::length(8.);
        frame.border.right = LengthPercentage::length(8.);

        let mut snapper = CellSnapper::new(size(8., 16.));
        snapper.set_viewport_width(640.);
        let frame = snapper.request_layout(
            &mut tree,
            frame,
            &Style::default(),
            &[LayoutId::from(column), LayoutId::from(footer)],
        );

        let width = |node| tree.style(node).unwrap().size.width;
        assert_eq!(
            tree.style(frame.into()).unwrap().min_size.width,
            Dimension::length(640.)
        );
        assert_eq!(width(results), Dimension::length(624.));
        assert_eq!(width(footer), Dimension::length(320.));
    }

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
