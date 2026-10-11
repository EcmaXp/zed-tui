use gpui::{Along, Axis, Bounds, Pixels, Size, point, px, size};

use super::{PaneAxisElement, child_bounds, separator_row};
use crate::cell_layout::{cell_start, resize_handle_span};

fn cell_size() -> Size<Pixels> {
    size(px(8.), px(16.))
}

fn first_child(flexes: &[f32], bounds: Bounds<Pixels>, axis: Axis) -> Bounds<Pixels> {
    let separator_row = separator_row(
        axis,
        bounds.size.along(axis),
        flexes.len(),
        Some(cell_size()),
    );
    let (first, _) = child_bounds(bounds, axis, flexes, separator_row)
        .next()
        .expect("an axis has children");
    first
}

fn line_cell(flexes: &[f32], bounds: Bounds<Pixels>, axis: Axis) -> i32 {
    let cell = cell_size().along(axis);
    let first = first_child(flexes, bounds, axis);
    (cell_start(first.origin.along(axis) + first.size.along(axis), cell) / cell) as i32
}

fn drag_to(flexes: &mut Vec<f32>, bounds: Bounds<Pixels>, axis: Axis, target: i32) {
    let cell = cell_size().along(axis);
    let pointer = bounds
        .center()
        .apply_along(axis, |_| cell * (target as f32 + 0.5));
    let first = first_child(flexes, bounds, axis);
    PaneAxisElement::resize_flexes(
        flexes,
        0,
        axis,
        pointer,
        first.origin,
        bounds,
        Some(cell_size()),
    );
}

#[test]
fn gui_resize_handles_straddle_the_boundary() {
    assert_eq!(resize_handle_span(None, px(4.)), (px(-2.), px(4.)));
    assert_eq!(resize_handle_span(None, px(6.)), (px(-3.), px(6.)));
}

#[test]
fn tui_resize_handles_cover_the_one_cell_after_the_boundary() {
    assert_eq!(resize_handle_span(Some(px(8.)), px(4.)), (px(0.), px(8.)));
    assert_eq!(resize_handle_span(Some(px(16.)), px(6.)), (px(0.), px(16.)));
}

#[test]
fn gui_layout_keeps_rounded_flex_extents_without_separators() {
    let bounds = Bounds::new(point(px(3.), px(5.)), size(px(1001.), px(703.)));
    let flexes = [0.7, 1.45, 0.85];
    for axis in [Axis::Horizontal, Axis::Vertical] {
        let space_per_flex = bounds.size.along(axis) / flexes.len() as f32;
        let mut origin = bounds.origin;
        for ((child, bounding_box), flex) in child_bounds(bounds, axis, &flexes, None).zip(flexes) {
            let expected_size = bounds
                .size
                .apply_along(axis, |_| space_per_flex * flex)
                .map(|extent| extent.round());
            assert_eq!(child, Bounds::new(origin, expected_size));
            assert_eq!(bounding_box, child);
            origin = origin.apply_along(axis, |value| value + expected_size.along(axis));
        }
    }
}

#[test]
fn dragging_a_divider_lands_its_line_on_the_target_cell() {
    let cases = [
        (Axis::Horizontal, 90, 11),
        (Axis::Horizontal, 91, 11),
        (Axis::Vertical, 37, 8),
        (Axis::Vertical, 38, 8),
    ];
    for (axis, cells, margin) in cases {
        let bounds = Bounds::new(
            point(px(24.), px(16.)),
            size(px(8.) * cells, px(16.) * cells),
        );
        let first = (bounds.origin.along(axis) / cell_size().along(axis)) as i32;
        let targets = first + margin..first + cells as i32 - margin;
        let initial = vec![1.; 2];
        for grab in targets.clone() {
            let mut grabbed = initial.clone();
            drag_to(&mut grabbed, bounds, axis, grab);
            assert_eq!(line_cell(&grabbed, bounds, axis), grab);
            for target in targets.clone() {
                let mut flexes = grabbed.clone();
                drag_to(&mut flexes, bounds, axis, target);
                assert_eq!(
                    line_cell(&flexes, bounds, axis),
                    target,
                    "{axis:?} at {cells} cells, from {grab} to {target}"
                );
            }
        }
    }
}

#[test]
fn grabbing_a_divider_without_moving_changes_nothing() {
    for (axis, cells) in [
        (Axis::Horizontal, 90),
        (Axis::Horizontal, 91),
        (Axis::Vertical, 37),
        (Axis::Vertical, 38),
    ] {
        let bounds = Bounds::new(
            point(px(24.), px(16.)),
            size(px(8.) * cells, px(16.) * cells),
        );
        for flexes in [vec![1., 1.], vec![0.9137, 1.0863], vec![1.1, 0.8, 1.1]] {
            let mut grabbed = flexes.clone();
            drag_to(&mut grabbed, bounds, axis, line_cell(&flexes, bounds, axis));
            assert_eq!(grabbed, flexes, "{axis:?} at {cells} cells");
        }
    }
}

#[test]
fn stacked_panes_reserve_one_separator_row_between_them() {
    let row = cell_size().height;
    for rows in [20, 21, 37, 38] {
        for len in [2, 3, 4] {
            let bounds = Bounds::new(point(px(24.), px(32.)), size(px(640.), row * rows));
            let flexes = vec![1.; len];
            let layout =
                child_bounds(bounds, Axis::Vertical, &flexes, Some(row)).collect::<Vec<_>>();
            let extents = layout
                .iter()
                .map(|(child, _)| child.size.height)
                .sum::<Pixels>();
            assert_eq!(extents + row * (len - 1), bounds.size.height);
            for (child, _) in &layout {
                assert_eq!(child.top(), cell_start(child.top(), row));
                assert_eq!(child.size.height, cell_start(child.size.height, row));
            }
            for pair in layout.windows(2) {
                let [(upper, upper_box), (lower, lower_box)] = pair else {
                    continue;
                };
                assert_eq!(lower.top() - upper.bottom(), row);
                assert_eq!(upper_box.top(), upper.top());
                assert_eq!(upper_box.bottom(), lower.top());
                assert_eq!(lower_box.top(), lower.top());
            }
            if let Some((last, last_box)) = layout.last() {
                assert_eq!(last_box, last);
                assert_eq!(last.bottom(), bounds.bottom());
            }
        }
    }
}

#[test]
fn stacked_panes_drop_separators_when_every_pane_cannot_keep_a_row() {
    let row = cell_size().height;
    for (rows, len) in [(2, 2), (4, 3), (6, 4)] {
        assert_eq!(
            separator_row(Axis::Vertical, row * rows, len, Some(cell_size())),
            None
        );
        assert_eq!(
            separator_row(Axis::Vertical, row * (rows + 1), len, Some(cell_size())),
            Some(row)
        );
    }
    assert_eq!(
        separator_row(Axis::Horizontal, px(800.), 2, Some(cell_size())),
        None
    );
    assert_eq!(separator_row(Axis::Vertical, px(800.), 2, None), None);
}
