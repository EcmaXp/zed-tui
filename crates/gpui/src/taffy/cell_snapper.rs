use super::{EXPECT_MESSAGE, LayoutId, NodeContext, NodeMeasureFn};
use crate::{
    AbsoluteLength, Bounds, DefiniteLength, Edges, Length, Pixels, Point, Size, Style, Window,
    size, util::round_half_toward_zero,
};
use collections::FxHashMap;
use taffy::{TaffyTree, TraversePartialTree as _};

pub(super) struct CellSnapper {
    cell_size: Size<f32>,
    viewport_width: f32,
    cell_nodes: FxHashMap<LayoutId, CellNode>,
    nested_rule_spacing_owners: FxHashMap<(LayoutId, Side), LayoutId>,
}

#[derive(Clone, Copy)]
struct CellNode {
    is_empty: bool,
    left: Edge,
    right: Edge,
    hairline_width: Option<f32>,
}

impl CellNode {
    fn edge(&self, side: Side) -> Edge {
        match side {
            Side::Left => self.left,
            Side::Right => self.right,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Edge {
    Content,
    Blank,
    CoveredPadding,
    Rule,
    RuleSpacing(Spacing),
    NestedRuleSpacing(Spacing),
}

impl Edge {
    fn is_blank(self) -> bool {
        matches!(
            self,
            Self::Blank | Self::CoveredPadding | Self::RuleSpacing(_) | Self::NestedRuleSpacing(_)
        )
    }

    fn is_visible_blank(self) -> bool {
        self != Self::CoveredPadding && self.is_blank()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Spacing {
    Padding,
    Margin,
}

struct Snapped {
    node: CellNode,
    nested_rule_spacing_owners: Vec<(Side, LayoutId)>,
    dropped_spacings: Vec<(LayoutId, Side, Spacing)>,
}

impl CellSnapper {
    pub(super) fn new(cell_size: Size<f32>) -> Self {
        Self {
            cell_size,
            viewport_width: f32::INFINITY,
            cell_nodes: FxHashMap::default(),
            nested_rule_spacing_owners: FxHashMap::default(),
        }
    }

    pub(super) fn set_viewport_width(&mut self, viewport_width: f32) {
        self.viewport_width = viewport_width;
    }

    pub(super) fn clear(&mut self) {
        self.cell_nodes.clear();
        self.nested_rule_spacing_owners.clear();
    }

    pub(super) fn request_layout(
        &mut self,
        tree: &mut TaffyTree<NodeContext>,
        mut taffy_style: taffy::style::Style,
        style: &Style,
        children: &[LayoutId],
    ) -> LayoutId {
        let snapped = self.snap(&mut taffy_style, style, children);
        for (node, side, spacing) in snapped.dropped_spacings {
            drop_spacing(tree, node, side, spacing);
        }
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
        let id = if children.is_empty() {
            tree.new_leaf(taffy_style)
        } else {
            tree.new_with_children(taffy_style, LayoutId::to_taffy_slice(children))
        }
        .expect(EXPECT_MESSAGE)
        .into();
        self.cell_nodes.insert(id, snapped.node);
        for (side, owner) in snapped.nested_rule_spacing_owners {
            self.nested_rule_spacing_owners.insert((id, side), owner);
        }
        id
    }

    pub(super) fn request_measured_layout(
        &mut self,
        tree: &mut TaffyTree<NodeContext>,
        mut taffy_style: taffy::style::Style,
        style: &Style,
        measure: NodeMeasureFn,
    ) -> LayoutId {
        let cell_node = self.snap_measured(&mut taffy_style, style);
        let id = tree
            .new_leaf_with_context(taffy_style, NodeContext { measure })
            .expect(EXPECT_MESSAGE)
            .into();
        self.cell_nodes.insert(id, cell_node);
        id
    }

    fn snap(
        &self,
        taffy_style: &mut taffy::style::Style,
        style: &Style,
        children: &[LayoutId],
    ) -> Snapped {
        let unsnapped = HorizontalSpacing::of(taffy_style);
        let hairline_width = positive_length(taffy_style.size.width)
            .filter(|width| *width < self.cell_size.width / 2.0);
        snap_to_cells(taffy_style, style, self.cell_size, self.viewport_width);
        let content: Vec<LayoutId> = children
            .iter()
            .copied()
            .filter(|child| !self.cell_nodes.get(child).is_some_and(|node| node.is_empty))
            .collect();
        let is_framed = style.is_framed_surface();
        let draws_rules = !self.is_toggle_box(taffy_style)
            && style
                .border_color
                .is_some_and(|color| !color.is_transparent());
        let ruled_left = draws_rules && !style.border_widths.left.is_zero();
        let ruled_right = draws_rules && !is_framed && !style.border_widths.right.is_zero();
        let (keeps_left_rule_padding, keeps_right_rule_padding) = self.avoid_doubled_blank_columns(
            taffy_style,
            unsnapped,
            &content,
            ruled_left,
            ruled_right,
        );
        if is_framed {
            keep_left_frame_edge_only(taffy_style, self.cell_size.width);
        }
        let dropped_spacings = self.redundant_rule_spacings(taffy_style, &content);
        let node = self.cell_node(taffy_style, &content, false);
        let is_in_flow = taffy_style.position != taffy::style::Position::Absolute;
        let is_rule_margin = |margin: taffy::style::LengthPercentageAuto| {
            is_in_flow
                && positive_length(margin).is_some_and(|margin| margin <= self.cell_size.width)
        };
        let edge = |edge, is_ruled, margin, keeps_rule_padding| {
            if hairline_width.is_some() {
                Edge::Content
            } else if is_ruled && is_rule_margin(margin) {
                Edge::RuleSpacing(Spacing::Margin)
            } else if is_ruled {
                Edge::Rule
            } else if keeps_rule_padding {
                Edge::RuleSpacing(Spacing::Padding)
            } else {
                edge
            }
        };
        let node = CellNode {
            left: edge(
                node.left,
                ruled_left,
                taffy_style.margin.left,
                keeps_left_rule_padding,
            ),
            right: edge(
                node.right,
                ruled_right,
                taffy_style.margin.right,
                keeps_right_rule_padding,
            ),
            hairline_width,
            ..node
        };
        let nested_rule_spacing_owners =
            [(Side::Left, content.first()), (Side::Right, content.last())]
                .into_iter()
                .filter(|(side, _)| matches!(node.edge(*side), Edge::NestedRuleSpacing(_)))
                .filter_map(|(side, edge_child)| {
                    Some((side, self.rule_spacing_owner(*edge_child?, side)?))
                })
                .collect();
        Snapped {
            node,
            nested_rule_spacing_owners,
            dropped_spacings,
        }
    }

    fn is_toggle_box(&self, style: &taffy::style::Style) -> bool {
        let fits = |length: taffy::style::Dimension, limit: f32| {
            positive_length(length).is_some_and(|length| length <= limit)
        };
        fits(
            style.size.width,
            Style::MAX_TOGGLE_BOX_COLUMNS as f32 * self.cell_size.width,
        ) && fits(style.size.height, self.cell_size.height)
    }

    fn snap_measured(&self, taffy_style: &mut taffy::style::Style, style: &Style) -> CellNode {
        snap_to_cells(taffy_style, style, self.cell_size, self.viewport_width);
        if style.is_framed_surface() {
            keep_left_frame_edge_only(taffy_style, self.cell_size.width);
        }
        self.cell_node(taffy_style, &[], true)
    }

    pub(super) fn snap_measured_size(&self, measured: Size<f32>) -> Size<f32> {
        size(
            ceil_to_cell(measured.width, self.cell_size.width),
            round_extent_to_cells(measured.height, self.cell_size.height),
        )
    }

    pub(super) fn snap_bounds(
        &self,
        id: LayoutId,
        origin: Point<f32>,
        far: Point<f32>,
        layout_size: taffy::geometry::Size<f32>,
    ) -> Bounds<f32> {
        let (mut x, mut width) =
            snap_axis(origin.x..far.x, self.cell_size.width, layout_size.width);
        let (y, height) = snap_axis(origin.y..far.y, self.cell_size.height, layout_size.height);
        if let Some(hairline_width) = self
            .cell_nodes
            .get(&id)
            .and_then(|node| node.hairline_width)
        {
            x += (width - hairline_width) / 2.0;
            width = hairline_width;
        }
        Bounds::new(Point { x, y }, size(width, height))
    }

    fn avoid_doubled_blank_columns(
        &self,
        style: &mut taffy::style::Style,
        unsnapped: HorizontalSpacing,
        content: &[LayoutId],
        ruled_left: bool,
        ruled_right: bool,
    ) -> (bool, bool) {
        let Some(flow) = Flow::of(style, content) else {
            return (false, false);
        };
        let round = |value| round_to_cell(value, self.cell_size.width);
        let is_blank_inside = |child: &LayoutId, side, is_ruled| {
            let edge = self.edge(*child, side);
            if is_ruled {
                edge.is_visible_blank()
            } else {
                edge.is_blank()
            }
        };
        let mut restore_left = positive_length(unsnapped.padding_left).is_some()
            && flow
                .leading
                .iter()
                .all(|child| is_blank_inside(child, Side::Left, ruled_left));
        let mut restore_right = positive_length(unsnapped.padding_right).is_some()
            && flow
                .trailing
                .iter()
                .all(|child| is_blank_inside(child, Side::Right, ruled_right));
        if !flow.is_row
            && unsnapped.padding_left == unsnapped.padding_right
            && restore_left != restore_right
        {
            restore_left = false;
            restore_right = false;
        }
        if restore_left {
            style.padding.left = snap_length(unsnapped.padding_left, round);
        }
        if restore_right {
            style.padding.right = snap_length(unsnapped.padding_right, round);
        }
        if flow.is_row
            && positive_length(unsnapped.gap_width).is_some()
            && flow.pairs().all(|(before, after)| {
                self.edge(before, Side::Right).is_blank() || self.edge(after, Side::Left).is_blank()
            })
        {
            style.gap.width = snap_length(unsnapped.gap_width, round);
        }
        let keeps_rule_padding = |padding: taffy::style::LengthPercentage,
                                  unsnapped_padding: taffy::style::LengthPercentage,
                                  edge_children: &[LayoutId],
                                  side| {
            positive_length(padding).is_some()
                && positive_length(snap_length(unsnapped_padding, round)).is_none()
                && edge_children
                    .iter()
                    .all(|child| self.edge(*child, side) == Edge::Rule)
        };
        (
            keeps_rule_padding(
                style.padding.left,
                unsnapped.padding_left,
                flow.leading,
                Side::Left,
            ),
            keeps_rule_padding(
                style.padding.right,
                unsnapped.padding_right,
                flow.trailing,
                Side::Right,
            ),
        )
    }

    fn redundant_rule_spacings(
        &self,
        style: &taffy::style::Style,
        content: &[LayoutId],
    ) -> Vec<(LayoutId, Side, Spacing)> {
        let mut redundant = Vec::new();
        let Some(flow) = Flow::of(style, content) else {
            return redundant;
        };
        let mut drop_nested = |child, edge, side| {
            if let Edge::NestedRuleSpacing(spacing) = edge
                && let Some(owner) = self.rule_spacing_owner(child, side)
            {
                redundant.push((owner, side, spacing));
            }
        };
        let is_blank = |length: taffy::style::LengthPercentage| positive_length(length).is_some();
        if is_blank(style.padding.left) {
            for child in flow.leading {
                drop_nested(*child, self.edge(*child, Side::Left), Side::Left);
            }
        }
        if is_blank(style.padding.right) {
            for child in flow.trailing {
                drop_nested(*child, self.edge(*child, Side::Right), Side::Right);
            }
        }
        if flow.is_row {
            let gap_is_blank = is_blank(style.gap.width);
            for (before, after) in flow.pairs() {
                let before_edge = self.edge(before, Side::Right);
                let after_edge = self.edge(after, Side::Left);
                if gap_is_blank || before_edge.is_blank() {
                    drop_nested(after, after_edge, Side::Left);
                }
                if gap_is_blank || after_edge == Edge::Blank {
                    drop_nested(before, before_edge, Side::Right);
                }
            }
        }
        redundant
    }

    fn cell_node(
        &self,
        style: &taffy::style::Style,
        content: &[LayoutId],
        is_measured: bool,
    ) -> CellNode {
        let is_empty = style.display == taffy::style::Display::None
            || (style.size.width.is_auto()
                && style.min_size.width.is_auto()
                && !is_measured
                && content.is_empty());
        let is_empty_slot = content.is_empty()
            && style.display == taffy::style::Display::Flex
            && length_value(style.size.width).is_some();
        let edge =
            |padding: taffy::style::LengthPercentage, edge_child: Option<&LayoutId>, side| {
                if positive_length(padding).is_some() || is_empty_slot {
                    Edge::Blank
                } else {
                    edge_child.map_or(Edge::Content, |child| self.edge(*child, side))
                }
            };
        let left = if self.overflow_covers_left_padding(style, content) {
            Edge::CoveredPadding
        } else {
            edge(style.padding.left, content.first(), Side::Left)
        };
        CellNode {
            is_empty,
            left,
            right: edge(style.padding.right, content.last(), Side::Right),
            hairline_width: None,
        }
    }

    fn overflow_covers_left_padding(
        &self,
        style: &taffy::style::Style,
        content: &[LayoutId],
    ) -> bool {
        let pads_at_most_one_cell = || {
            positive_length(style.padding.left)
                .is_some_and(|padding| padding <= self.cell_size.width)
        };
        let leaves_no_room_inside_padding = || {
            positive_length(style.size.width).is_some_and(|width| {
                width <= horizontal_edges(&style.padding) + horizontal_edges(&style.border)
            })
        };
        !content.is_empty()
            && style.justify_content == Some(taffy::style::JustifyContent::CENTER)
            && is_row(style)
            && pads_at_most_one_cell()
            && leaves_no_room_inside_padding()
    }

    fn edge(&self, id: LayoutId, side: Side) -> Edge {
        match self.cell_nodes.get(&id).map(|node| node.edge(side)) {
            Some(Edge::RuleSpacing(spacing)) => Edge::NestedRuleSpacing(spacing),
            Some(edge) => edge,
            None => Edge::Content,
        }
    }

    fn rule_spacing_owner(&self, id: LayoutId, side: Side) -> Option<LayoutId> {
        match self.cell_nodes.get(&id)?.edge(side) {
            Edge::RuleSpacing(_) => Some(id),
            Edge::NestedRuleSpacing(_) => self.nested_rule_spacing_owners.get(&(id, side)).copied(),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Side {
    Left,
    Right,
}

struct Flow<'a> {
    content: &'a [LayoutId],
    is_row: bool,
    is_reversed: bool,
    leading: &'a [LayoutId],
    trailing: &'a [LayoutId],
}

impl<'a> Flow<'a> {
    fn of(style: &taffy::style::Style, content: &'a [LayoutId]) -> Option<Self> {
        let (first, last) = (content.first()?, content.last()?);
        let is_row = is_row(style);
        let is_reversed = style.flex_direction == taffy::style::FlexDirection::RowReverse;
        let (leading, trailing) = match (is_row, is_reversed) {
            (false, _) => (content, content),
            (true, false) => (std::slice::from_ref(first), std::slice::from_ref(last)),
            (true, true) => (std::slice::from_ref(last), std::slice::from_ref(first)),
        };
        Some(Self {
            content,
            is_row,
            is_reversed,
            leading,
            trailing,
        })
    }

    fn pairs(&self) -> impl Iterator<Item = (LayoutId, LayoutId)> + '_ {
        self.content.windows(2).map(|pair| {
            if self.is_reversed {
                (pair[1], pair[0])
            } else {
                (pair[0], pair[1])
            }
        })
    }
}

fn is_row(style: &taffy::style::Style) -> bool {
    style.display == taffy::style::Display::Flex
        && matches!(
            style.flex_direction,
            taffy::style::FlexDirection::Row | taffy::style::FlexDirection::RowReverse
        )
}

fn drop_spacing(tree: &mut TaffyTree<NodeContext>, node: LayoutId, side: Side, spacing: Spacing) {
    let mut style = tree.style(node.into()).expect(EXPECT_MESSAGE).clone();
    let zero_padding = taffy::style::LengthPercentage::length(0.);
    let zero_margin = taffy::style::LengthPercentageAuto::length(0.);
    match (spacing, side) {
        (Spacing::Padding, Side::Left) => style.padding.left = zero_padding,
        (Spacing::Padding, Side::Right) => style.padding.right = zero_padding,
        (Spacing::Margin, Side::Left) => style.margin.left = zero_margin,
        (Spacing::Margin, Side::Right) => style.margin.right = zero_margin,
    }
    tree.set_style(node.into(), style).expect(EXPECT_MESSAGE);
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

#[derive(Clone, Copy)]
struct HorizontalSpacing {
    padding_left: taffy::style::LengthPercentage,
    padding_right: taffy::style::LengthPercentage,
    gap_width: taffy::style::LengthPercentage,
}

impl HorizontalSpacing {
    fn of(style: &taffy::style::Style) -> Self {
        Self {
            padding_left: style.padding.left,
            padding_right: style.padding.right,
            gap_width: style.gap.width,
        }
    }
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

fn keep_left_frame_edge_only(style: &mut taffy::style::Style, cell_width: f32) {
    let zero = taffy::style::LengthPercentage::length(0.);
    let at_most_one_cell = |value: f32| value.min(cell_width);
    style.border.top = zero;
    style.border.right = zero;
    style.border.bottom = zero;
    style.padding.top = zero;
    style.padding.bottom = zero;
    style.padding.left = snap_length(style.padding.left, at_most_one_cell);
    style.padding.right = snap_length(style.padding.right, at_most_one_cell);
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

impl Style {
    #[expect(missing_docs)]
    pub const MAX_TOGGLE_BOX_COLUMNS: usize = 2;

    pub(crate) fn is_framed_surface(&self) -> bool {
        !self.box_shadow.is_empty()
            && !self.border_widths.any(|width| width.is_zero())
            && self
                .border_color
                .is_some_and(|color| !color.is_transparent())
            && self.size.height == Length::Auto
    }

    pub(crate) fn painted_border_widths(&self, window: &Window) -> Edges<AbsoluteLength> {
        if self.is_framed_surface()
            && let Some(cell_size) = window.text_system().cell_size()
        {
            Edges {
                left: cell_size.width.into(),
                ..Edges::<AbsoluteLength>::zero()
            }
        } else {
            self.border_widths
        }
    }
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
mod fork_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Edges;
    use taffy::geometry::{Rect as TaffyRect, Size as TaffySize};
    use taffy::style::LengthPercentage;

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
    fn columns_keep_symmetric_padding_when_only_one_side_is_blank() {
        use taffy::style::LengthPercentage;
        let mut snapper = CellSnapper::new(size(8., 16.));
        let mut tree = TaffyTree::<NodeContext>::new();
        let child = LayoutId::from(tree.new_leaf(taffy::style::Style::default()).unwrap());
        snapper.cell_nodes.insert(
            child,
            CellNode {
                is_empty: false,
                left: Edge::Content,
                right: Edge::Blank,
                hairline_width: None,
            },
        );
        let padded = |direction| {
            let mut style = taffy::style::Style {
                flex_direction: direction,
                ..Default::default()
            };
            style.padding.left = LengthPercentage::length(4.);
            style.padding.right = LengthPercentage::length(4.);
            style
        };
        for (direction, expected_right) in [
            (taffy::style::FlexDirection::Column, 8.),
            (taffy::style::FlexDirection::Row, 0.),
        ] {
            let unsnapped_style = padded(direction);
            let unsnapped = HorizontalSpacing::of(&unsnapped_style);
            let mut style = unsnapped_style.clone();
            snap_style_to_cells(&mut style, snapper.cell_size);
            snapper.avoid_doubled_blank_columns(&mut style, unsnapped, &[child], false, false);
            assert_eq!(
                style.padding.left,
                LengthPercentage::length(8.),
                "{direction:?}"
            );
            assert_eq!(
                style.padding.right,
                LengthPercentage::length(expected_right),
                "{direction:?}"
            );
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

    fn framed_style(shadowed: bool, borders: Edges<AbsoluteLength>) -> Style {
        let mut style = Style::default();
        style.border_widths = borders;
        style.border_color = Some(crate::hsla(0., 0., 0.5, 1.));
        if shadowed {
            style.box_shadow = vec![crate::BoxShadow {
                color: crate::hsla(0., 0., 0., 0.12),
                offset: crate::point(Pixels::ZERO, Pixels(2.)),
                blur_radius: Pixels(4.),
                spread_radius: Pixels::ZERO,
                inset: false,
            }];
        }
        style
    }

    fn padding(left: f32, right: f32, top: f32, bottom: f32) -> TaffyRect<LengthPercentage> {
        TaffyRect {
            left: LengthPercentage::length(left),
            right: LengthPercentage::length(right),
            top: LengthPercentage::length(top),
            bottom: LengthPercentage::length(bottom),
        }
    }

    fn snapped_box(
        snapper: &CellSnapper,
        style: &Style,
        padding: TaffyRect<LengthPercentage>,
        children: &[LayoutId],
    ) -> taffy::style::Style {
        let border =
            |width: AbsoluteLength| LengthPercentage::length(width.to_pixels(Pixels(16.)).0);
        let mut taffy_style = taffy::style::Style {
            border: TaffyRect {
                left: border(style.border_widths.left),
                right: border(style.border_widths.right),
                top: border(style.border_widths.top),
                bottom: border(style.border_widths.bottom),
            },
            padding,
            ..Default::default()
        };
        snapper.snap(&mut taffy_style, style, children);
        taffy_style
    }

    fn snapped_menu_box(style: &Style) -> taffy::style::Style {
        snapped_box(
            &CellSnapper::new(size(8., 16.)),
            style,
            padding(0., 0., 4., 4.),
            &[],
        )
    }

    fn one_pixel_borders() -> Edges<AbsoluteLength> {
        Edges::all(AbsoluteLength::Pixels(Pixels(1.)))
    }

    #[test]
    fn framed_surfaces_keep_only_a_left_border_column() {
        let snapped = snapped_menu_box(&framed_style(true, one_pixel_borders()));
        assert_eq!(snapped.border.top, LengthPercentage::length(0.));
        assert_eq!(snapped.border.right, LengthPercentage::length(0.));
        assert_eq!(snapped.border.bottom, LengthPercentage::length(0.));
        assert_eq!(snapped.border.left, LengthPercentage::length(8.));
    }

    #[test]
    fn framed_surfaces_drop_vertical_padding_and_keep_at_most_one_padding_column() {
        let snapper = CellSnapper::new(size(8., 16.));
        let roomy = padding(24., 4., 24., 24.);
        let framed = snapped_box(
            &snapper,
            &framed_style(true, one_pixel_borders()),
            roomy,
            &[],
        );
        assert_eq!(framed.padding, padding(8., 8., 0., 0.));

        let unframed = snapped_box(
            &snapper,
            &framed_style(false, one_pixel_borders()),
            roomy,
            &[],
        );
        assert_eq!(unframed.padding, padding(24., 8., 16., 16.));
    }

    #[test]
    fn framed_surface_padding_stays_one_column_when_children_are_blank_at_their_edges() {
        let mut snapper = CellSnapper::new(size(8., 16.));
        let mut tree = TaffyTree::<NodeContext>::new();
        let child = LayoutId::from(tree.new_leaf(taffy::style::Style::default()).unwrap());
        snapper.cell_nodes.insert(
            child,
            CellNode {
                is_empty: false,
                left: Edge::Blank,
                right: Edge::Blank,
                hairline_width: None,
            },
        );
        let snapped = snapped_box(
            &snapper,
            &framed_style(true, one_pixel_borders()),
            padding(24., 24., 0., 0.),
            &[child],
        );
        assert_eq!(snapped.padding, padding(8., 8., 0., 0.));
    }

    #[test]
    fn bordered_boxes_without_a_shadow_keep_collapsed_top_and_bottom_borders() {
        let snapped = snapped_menu_box(&framed_style(false, one_pixel_borders()));
        assert_eq!(snapped.border.top, LengthPercentage::length(0.));
        assert_eq!(snapped.border.bottom, LengthPercentage::length(0.));
        assert_eq!(snapped.border.right, LengthPercentage::length(8.));
    }

    #[test]
    fn framed_surfaces_with_a_fixed_height_keep_collapsed_top_and_bottom_borders() {
        let mut style = framed_style(true, one_pixel_borders());
        style.size.height = Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Pixels(
            Pixels(32.),
        )));
        let snapped = snapped_menu_box(&style);
        assert_eq!(snapped.border.top, LengthPercentage::length(0.));
        assert_eq!(snapped.border.right, LengthPercentage::length(8.));
    }

    #[test]
    fn shadowed_boxes_with_partial_borders_keep_collapsed_top_and_bottom_borders() {
        let borders = Edges {
            bottom: AbsoluteLength::Pixels(Pixels(1.)),
            ..Edges::all(AbsoluteLength::Pixels(Pixels::ZERO))
        };
        let snapped = snapped_menu_box(&framed_style(true, borders));
        assert_eq!(snapped.border.bottom, LengthPercentage::length(0.));
    }
}
