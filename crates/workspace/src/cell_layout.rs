use gpui::{
    Along, App, Axis, Bounds, Context, FocusHandle, Focusable as _, Pixels, Point, Size, Window,
};

use crate::{Event, Pane, Workspace, dock::DockPosition};

pub(crate) fn resize_handle_span(cell: Option<Pixels>, gui_extent: Pixels) -> (Pixels, Pixels) {
    match cell {
        Some(cell) => (Pixels::ZERO, cell),
        None => (-gui_extent / 2., gui_extent),
    }
}

pub(crate) fn cell_start(position: Pixels, cell: Pixels) -> Pixels {
    cell * (position / cell).floor()
}

pub(crate) fn separator_row(
    axis: Axis,
    axis_extent: Pixels,
    len: usize,
    cell_size: Option<Size<Pixels>>,
) -> Option<Pixels> {
    cell_size
        .filter(|_| axis == Axis::Vertical)
        .map(|cell_size| cell_size.height)
        .filter(|row| axis_extent >= *row * (2 * len).saturating_sub(1))
}

pub(crate) fn flex_space(axis_extent: Pixels, separator_row: Option<Pixels>, len: usize) -> Pixels {
    axis_extent - separator_row.unwrap_or_default() * len.saturating_sub(1)
}

fn flex_extent(flexes: &[f32], ix: usize, container_extent: Pixels) -> Pixels {
    container_extent * (flexes[ix] / flexes.len() as f32)
}

fn snapped_extent(extent: Pixels, separator_row: Option<Pixels>) -> Pixels {
    match separator_row {
        Some(row) => row * (extent / row).round(),
        None => extent.round(),
    }
}

pub(crate) fn child_bounds(
    bounds: Bounds<Pixels>,
    axis: Axis,
    flexes: &[f32],
    separator_row: Option<Pixels>,
) -> impl Iterator<Item = (Bounds<Pixels>, Bounds<Pixels>)> + '_ {
    let len = flexes.len();
    let separator = separator_row.unwrap_or_default();
    let space_per_flex = flex_space(bounds.size.along(axis), separator_row, len) / len as f32;
    let axis_end = bounds.origin.along(axis) + bounds.size.along(axis);
    flexes
        .iter()
        .enumerate()
        .scan(bounds.origin, move |origin, (ix, child_flex)| {
            let is_last = ix + 1 == len;
            let extent = match separator_row {
                Some(_) if is_last => (axis_end - origin.along(axis)).max(Pixels::ZERO),
                _ => snapped_extent(space_per_flex * *child_flex, separator_row),
            };
            let size = bounds.size.apply_along(axis, |_| extent).map(|d| d.round());
            let child = Bounds {
                origin: *origin,
                size,
            };
            let bounding_box = if is_last {
                child
            } else {
                Bounds {
                    origin: *origin,
                    size: size.apply_along(axis, |extent| extent + separator),
                }
            };
            *origin = origin.apply_along(axis, |value| value + size.along(axis) + separator);
            Some((child, bounding_box))
        })
}

pub(crate) fn resize_metrics(
    flexes: &[f32],
    ix: usize,
    pointer: Point<Pixels>,
    child_start: Point<Pixels>,
    axis_bounds: Bounds<Pixels>,
    axis: Axis,
    cell_size: Option<Size<Pixels>>,
) -> (Size<Pixels>, Pixels, Pixels) {
    let axis_extent = axis_bounds.size.along(axis);
    let separator_row = separator_row(axis, axis_extent, flexes.len(), cell_size);
    let container_extent = flex_space(axis_extent, separator_row, flexes.len());
    let container_size = axis_bounds.size.apply_along(axis, |_| container_extent);
    let Some(cell_size) = cell_size else {
        let proposed_change =
            (pointer - child_start).along(axis) - flex_extent(flexes, ix, container_extent);
        return (container_size, container_extent, proposed_change);
    };
    let space_per_flex = container_extent / flexes.len() as f32;
    let cell = cell_size.along(axis);
    let proposed_change = child_bounds(axis_bounds, axis, flexes, separator_row)
        .nth(ix)
        .zip(flexes.get(ix))
        .map_or(Pixels::ZERO, |((child, _), child_flex)| {
            let child_origin = child.origin.along(axis);
            let visible_end = child_origin + child.size.along(axis);
            let cells_moved = cell_start(pointer.along(axis), cell) - cell_start(visible_end, cell);
            if cells_moved == Pixels::ZERO {
                return Pixels::ZERO;
            }
            visible_end + cells_moved - (child_origin + space_per_flex * *child_flex)
        });
    (container_size, space_per_flex, proposed_change)
}

pub(crate) fn dock_size_for_pointer(
    position: DockPosition,
    pointer: Point<Pixels>,
    workspace_bounds: Bounds<Pixels>,
    cell: Option<Size<Pixels>>,
) -> Pixels {
    match (position, cell) {
        (DockPosition::Left, None) => pointer.x - workspace_bounds.left(),
        (DockPosition::Right, None) => workspace_bounds.right() - pointer.x,
        (DockPosition::Bottom, None) => workspace_bounds.bottom() - pointer.y,
        (DockPosition::Left, Some(cell)) => {
            cell_start(pointer.x, cell.width) + cell.width - workspace_bounds.left()
        }
        (DockPosition::Right, Some(cell)) => {
            workspace_bounds.right() - cell_start(pointer.x, cell.width)
        }
        (DockPosition::Bottom, Some(cell)) => (workspace_bounds.bottom()
            - cell_start(pointer.y, cell.height))
        .min(workspace_bounds.size.height - cell.height),
    }
}

impl Workspace {
    pub(crate) fn zoom_hides_layout(&self, window: &Window) -> bool {
        self.zoomed.is_some() && window.text_system().cell_size().is_some()
    }

    fn zoomed_item_focus_handle(&self, cx: &App) -> Option<FocusHandle> {
        match self.zoomed_position {
            Some(position) => Some(
                self.dock_at_position(position)
                    .read(cx)
                    .active_panel()?
                    .panel_focus_handle(cx),
            ),
            None => {
                let pane = self.zoomed.as_ref()?.upgrade()?.downcast::<Pane>().ok()?;
                Some(pane.read(cx).focus_handle(cx))
            }
        }
    }

    pub(crate) fn reveal_focus_hidden_by_zoom(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.zoom_hides_layout(window) || window.focused(cx).is_none() {
            return false;
        }
        let Some(zoomed_focus) = self.zoomed_item_focus_handle(cx) else {
            return false;
        };
        let focus_left_zoomed_item = window
            .focus_lost_restore_target(cx)
            .is_some_and(|target| zoomed_focus.contains(&target, window));
        if !focus_left_zoomed_item {
            return false;
        }

        cx.defer_in(window, |this, window, cx| {
            match this.zoomed_position {
                Some(position) => this
                    .dock_at_position(position)
                    .update(cx, |dock, cx| dock.set_open(false, window, cx)),
                None => {
                    for pane in &this.panes {
                        pane.update(cx, |pane, cx| pane.set_zoomed(false, cx));
                    }
                }
            }
            this.zoomed = None;
            this.zoomed_position = None;
            cx.emit(Event::ZoomChanged);
            cx.notify();
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Bounds, point, px, size};

    use super::dock_size_for_pointer;
    use crate::dock::DockPosition;

    #[test]
    fn gui_dock_sizes_follow_the_pointer() {
        let bounds = Bounds::new(point(px(40.), px(30.)), size(px(1000.), px(700.)));
        let pointer = point(px(321.5), px(456.25));
        assert_eq!(
            dock_size_for_pointer(DockPosition::Left, pointer, bounds, None),
            pointer.x - bounds.left()
        );
        assert_eq!(
            dock_size_for_pointer(DockPosition::Right, pointer, bounds, None),
            bounds.right() - pointer.x
        );
        assert_eq!(
            dock_size_for_pointer(DockPosition::Bottom, pointer, bounds, None),
            bounds.bottom() - pointer.y
        );
    }

    #[test]
    fn tui_dock_borders_land_on_the_pointer_cell() {
        let cell = size(px(8.), px(16.));
        let bounds = Bounds::new(point(px(0.), px(16.)), size(px(8. * 120.), px(16. * 38.)));
        for column in 1..119 {
            let pointer = point(px(8. * column as f32 + 4.), bounds.center().y);
            let left = dock_size_for_pointer(DockPosition::Left, pointer, bounds, Some(cell));
            assert_eq!(bounds.left() + left - cell.width, px(8. * column as f32));
            let right = dock_size_for_pointer(DockPosition::Right, pointer, bounds, Some(cell));
            assert_eq!(bounds.right() - right, px(8. * column as f32));
        }
        for row in 2..39 {
            let pointer = point(bounds.center().x, px(16. * row as f32 + 8.));
            let bottom = dock_size_for_pointer(DockPosition::Bottom, pointer, bounds, Some(cell));
            assert_eq!(bounds.bottom() - bottom, px(16. * row as f32));
        }
    }

    #[test]
    fn tui_bottom_dock_leaves_one_center_row() {
        let cell = size(px(8.), px(16.));
        let bounds = Bounds::new(point(px(0.), px(16.)), size(px(8. * 120.), px(16. * 38.)));
        for row in 0..2 {
            let pointer = point(bounds.center().x, px(16. * row as f32 + 8.));
            let bottom = dock_size_for_pointer(DockPosition::Bottom, pointer, bounds, Some(cell));
            assert_eq!(bounds.bottom() - bottom, bounds.top() + cell.height);
        }
    }
}
