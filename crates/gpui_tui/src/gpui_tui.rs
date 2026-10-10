#![cfg(unix)]

mod atlas;
mod dispatcher;
mod grid;
mod platform;
mod rasterize;
mod text_system;
mod window;

use std::cell::RefCell;

use gpui::{Pixels, Point, Size, point, px, size};

pub use grid::{Cell, CellAttrs, CellGrid, Glyph, Rgb, UnderlineColor};
pub use platform::TuiPlatform;
pub use text_system::TuiTextSystem;

pub(crate) const CELL_WIDTH: f32 = 8.;
pub(crate) const CELL_HEIGHT: f32 = 16.;

pub(crate) fn size_for_cells(cols: u16, rows: u16) -> Size<Pixels> {
    size(px(cols as f32 * CELL_WIDTH), px(rows as f32 * CELL_HEIGHT))
}

pub(crate) fn cells_for_size(size: Size<Pixels>) -> (u16, u16) {
    (
        (size.width.as_f32() / CELL_WIDTH).floor().max(1.) as u16,
        (size.height.as_f32() / CELL_HEIGHT).floor().max(1.) as u16,
    )
}

pub fn cell_center(col: u16, row: u16) -> Point<Pixels> {
    device_cell_center(col.into(), row.into()).map(px)
}

pub(crate) fn device_cell_center(col: i32, row: i32) -> Point<f32> {
    point(
        col as f32 * CELL_WIDTH + CELL_WIDTH / 2.,
        row as f32 * CELL_HEIGHT + CELL_HEIGHT / 2.,
    )
}

pub(crate) fn with_taken<State, Value, Output>(
    cell: &RefCell<State>,
    slot: impl Fn(&mut State) -> &mut Option<Value>,
    call: impl FnOnce(&mut Value) -> Output,
) -> Option<Output> {
    let mut value = slot(&mut cell.borrow_mut()).take()?;
    let output = call(&mut value);
    slot(&mut cell.borrow_mut()).get_or_insert(value);
    Some(output)
}
