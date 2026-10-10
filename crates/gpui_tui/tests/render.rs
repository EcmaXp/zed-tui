#![cfg(unix)]

use std::{cell::RefCell, ops::Range, rc::Rc, time::Duration};

use gpui::{
    AnyWindowHandle, App, AppContext as _, Application, BorderStyle, Bounds, Context,
    ElementInputHandler, EntityInputHandler, FocusHandle, InteractiveElement as _, IntoElement,
    KeyDownEvent, Keystroke, Modifiers, PaintQuad, ParentElement, Pixels, PlatformInput, Render,
    StatefulInteractiveElement as _, Styled, UTF16Selection, Window, WindowOptions, canvas, div,
    fill, outline, point, prelude::FluentBuilder as _, px, rgb, size,
};
use gpui_tui::{CellAttrs, CellGrid, CursorPosition, CursorShape, Rgb, TuiPlatform};

struct Hello;

struct PaddedList;

impl Render for PaddedList {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x202020))
            .text_color(rgb(0xffffff))
            .text_size(px(10.))
            .line_height(gpui::relative(1.618))
            .children((0..5).map(|index| {
                div()
                    .border_1()
                    .border_color(rgb(0x404040))
                    .py(px(2.))
                    .child(format!("item {index}"))
            }))
    }
}

fn view_frame<V: Render>(cols: u16, rows: u16, view: impl FnOnce() -> V + 'static) -> CellGrid {
    first_frame(TuiPlatform::new(cols, rows), move |cx: &mut App| {
        cx.open_window(WindowOptions::default(), move |_, cx| {
            cx.new(move |_| view())
        })
        .expect("failed to open window");
    })
}

fn first_frame(platform: Rc<TuiPlatform>, open: impl FnOnce(&mut App) + 'static) -> CellGrid {
    let frames: Rc<RefCell<Vec<CellGrid>>> = Rc::default();
    platform.set_frame_sink({
        let frames = frames.clone();
        let platform = Rc::downgrade(&platform);
        move |grid| {
            frames.borrow_mut().push(grid);
            if let Some(platform) = platform.upgrade() {
                gpui::Platform::quit(&*platform);
            }
        }
    });
    Application::with_platform(platform).run(open);
    let mut frames = frames.borrow_mut();
    assert!(!frames.is_empty(), "no frame was presented");
    frames.remove(0)
}

impl Render for Hello {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(0x202020))
            .text_color(rgb(0xffffff))
            .text_size(px(16.))
            .line_height(px(16.))
            .child(div().h(px(16.)).child("hello terminal"))
            .child(div().h(px(16.)).bg(rgb(0xff0000)).child("second"))
    }
}

#[test]
fn renders_gpui_elements_into_cells() {
    let grid = view_frame(20, 4, || Hello);
    assert_eq!((grid.cols, grid.rows), (20, 4));
    assert_eq!(grid.row_text(0).trim_end(), "hello terminal");
    assert_eq!(grid.row_text(1).trim_end(), "second");
    let first = grid.cell(0, 0).expect("cell");
    assert_eq!(first.fg, Rgb::new(255, 255, 255));
    assert_eq!(first.bg, Rgb::new(0x20, 0x20, 0x20));
    assert_eq!(grid.cell(0, 1).expect("cell").bg, Rgb::new(255, 0, 0));
}

#[test]
fn list_items_take_one_row_each_and_side_borders_take_a_column() {
    let grid = view_frame(20, 8, || PaddedList);
    for index in 0..5 {
        assert_eq!(
            grid.row_text(index),
            format!("│item {index}            │"),
            "{}",
            grid.text()
        );
    }
}

struct ElevatedMenu;

impl Render for ElevatedMenu {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let menu = || {
            div()
                .w(px(80.))
                .bg(rgb(0x303030))
                .border_1()
                .border_color(rgb(0x808080))
                .py(px(4.))
                .child("item")
        };
        div()
            .size_full()
            .bg(rgb(0x202020))
            .text_color(rgb(0xffffff))
            .text_size(px(16.))
            .line_height(px(16.))
            .child(menu().shadow_md())
            .child(menu())
    }
}

fn trimmed_rows(grid: &CellGrid, rows: Range<u16>) -> Vec<String> {
    rows.map(|row| grid.row_text(row).trim_end().to_string())
        .collect()
}

#[test]
fn shadowed_bordered_surfaces_draw_only_a_wide_left_bar() {
    let grid = view_frame(20, 6, || ElevatedMenu);
    assert_eq!(
        trimmed_rows(&grid, 0..2),
        ["▌item", "│item    │"],
        "{}",
        grid.text()
    );
}

struct FramedMenu {
    padding_x: Pixels,
    padding_y: Pixels,
    items: &'static [&'static str],
}

impl Render for FramedMenu {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(0x202020))
            .text_color(rgb(0xffffff))
            .text_size(px(16.))
            .line_height(px(16.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(80.))
                    .bg(rgb(0x303030))
                    .border_1()
                    .border_color(rgb(0x808080))
                    .shadow_md()
                    .px(self.padding_x)
                    .py(self.padding_y)
                    .children(self.items.iter().copied()),
            )
            .child("after")
    }
}

fn framed_menu_frame(
    padding_x: Pixels,
    padding_y: Pixels,
    items: &'static [&'static str],
) -> CellGrid {
    first_frame(TuiPlatform::new(20, 6), move |cx: &mut App| {
        cx.open_window(WindowOptions::default(), |_, cx| {
            cx.new(|_| FramedMenu {
                padding_x,
                padding_y,
                items,
            })
        })
        .expect("failed to open window");
    })
}

#[test]
fn framed_surfaces_draw_a_wide_left_bar_on_every_row_and_no_corners() {
    let grid = framed_menu_frame(px(0.), px(4.), &["one", "two", "three"]);
    assert_eq!(
        trimmed_rows(&grid, 0..4),
        ["▌one", "▌two", "▌three", "after"],
        "{}",
        grid.text()
    );
    assert!(
        !grid.text().contains(['┌', '┐', '└', '┘', '─']),
        "{}",
        grid.text()
    );
}

struct ModalSurface;

impl Render for ModalSurface {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(0x202020))
            .text_color(rgb(0xffffff))
            .text_size(px(16.))
            .line_height(px(16.))
            .child(
                div().h(px(48.)).child(
                    div()
                        .flex()
                        .flex_col()
                        .when(window.text_system().cell_size().is_none(), |this| {
                            this.size_full()
                        })
                        .w(px(80.))
                        .bg(rgb(0x303030))
                        .border_1()
                        .border_color(rgb(0x808080))
                        .shadow_md()
                        .child("run")
                        .child("debug"),
                ),
            )
    }
}

#[test]
fn surfaces_that_keep_a_full_height_only_outside_a_cell_grid_draw_a_left_bar() {
    let grid = view_frame(20, 4, || ModalSurface);
    assert_eq!(
        trimmed_rows(&grid, 0..3),
        ["▌run", "▌debug", ""],
        "{}",
        grid.text()
    );
}

#[test]
fn framed_surface_padding_takes_no_rows() {
    let grid = framed_menu_frame(px(24.), px(24.), &["item"]);
    assert_eq!(
        trimmed_rows(&grid, 0..2),
        ["▌ item", "after"],
        "{}",
        grid.text()
    );
}

struct ClickableMenu {
    clicked: Rc<RefCell<Vec<usize>>>,
}

impl Render for ClickableMenu {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .text_size(px(16.))
            .line_height(px(16.))
            .child(
                div()
                    .w(px(80.))
                    .border_1()
                    .border_color(rgb(0x808080))
                    .shadow_md()
                    .py(px(4.))
                    .children(
                        ["first", "second"]
                            .into_iter()
                            .enumerate()
                            .map(|(index, label)| {
                                let clicked = self.clicked.clone();
                                div()
                                    .id(index)
                                    .child(label)
                                    .on_click(move |_, _, _| clicked.borrow_mut().push(index))
                            }),
                    ),
            )
    }
}

#[test]
fn clicks_on_the_first_row_of_a_framed_menu_reach_its_first_item() {
    let platform = TuiPlatform::new(20, 4);
    let clicked: Rc<RefCell<Vec<usize>>> = Rc::default();
    Application::with_platform(platform.clone()).run({
        let clicked = clicked.clone();
        move |cx: &mut App| {
            cx.open_window(WindowOptions::default(), |_, cx| {
                cx.new(|_| ClickableMenu { clicked })
            })
            .expect("failed to open window");
            cx.spawn(async move |cx| {
                let settle = Duration::from_millis(30);
                cx.background_executor().timer(settle).await;
                let position = gpui_tui::cell_center(3, 0);
                platform.handle_input(PlatformInput::MouseDown(gpui::MouseDownEvent {
                    button: gpui::MouseButton::Left,
                    position,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                    first_mouse: false,
                }));
                platform.handle_input(PlatformInput::MouseUp(gpui::MouseUpEvent {
                    button: gpui::MouseButton::Left,
                    position,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                }));
                cx.background_executor().timer(settle).await;
                cx.update(|cx| cx.quit());
            })
            .detach();
        }
    });
    assert_eq!(*clicked.borrow(), [0]);
}

struct CollapsedFrame;

impl Render for CollapsedFrame {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(0x202020))
            .text_color(rgb(0xffffff))
            .text_size(px(16.))
            .line_height(px(16.))
            .child(
                div()
                    .w(px(96.))
                    .border_1()
                    .border_color(rgb(0x808080))
                    .child("Run Debug")
                    .child("Spawn"),
            )
    }
}

#[test]
fn collapsed_frames_do_not_draw_rules_between_text_on_their_edge_rows() {
    let grid = view_frame(20, 4, || CollapsedFrame);
    let rows: Vec<String> = (0..2)
        .map(|row| grid.row_text(row).chars().take(12).collect())
        .collect();
    assert_eq!(rows, ["┌Run Debug ┐", "└Spawn     ┘"], "{}", grid.text());
}

#[derive(Clone, Copy)]
enum Leading {
    Nothing,
    HiddenItem,
    Divider,
}

#[derive(Default)]
struct GapBounds {
    child_lefts: Vec<Pixels>,
    divider: Option<Bounds<Pixels>>,
}

struct GapRow {
    padding: Pixels,
    leading: Leading,
    bounds: Rc<RefCell<GapBounds>>,
}

impl Render for GapRow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.bounds.take();
        let bounds = self.bounds.clone();
        let leading = match self.leading {
            Leading::Nothing => None,
            Leading::HiddenItem => Some(div().child(div().hidden()).into_any_element()),
            Leading::Divider => Some(
                canvas(
                    move |divider, _, _| bounds.borrow_mut().divider = Some(divider),
                    |_, _, _, _| {},
                )
                .w(px(1.))
                .h(px(16.))
                .into_any_element(),
            ),
        };
        div()
            .flex()
            .gap(px(2.5))
            .children(leading)
            .children((0..3).map(|_| {
                let bounds = self.bounds.clone();
                div().px(self.padding).child(
                    canvas(
                        move |child, _, _| bounds.borrow_mut().child_lefts.push(child.origin.x),
                        |_, _, _, _| {},
                    )
                    .w(px(8.))
                    .h(px(16.)),
                )
            }))
    }
}

fn gap_bounds(padding: Pixels, leading: Leading) -> GapBounds {
    let bounds: Rc<RefCell<GapBounds>> = Rc::default();
    first_frame(TuiPlatform::new(25, 1), {
        let bounds = bounds.clone();
        move |cx: &mut App| {
            cx.open_window(WindowOptions::default(), |_, cx| {
                cx.new(|_| GapRow {
                    padding,
                    leading,
                    bounds,
                })
            })
            .expect("failed to open window");
        }
    });
    bounds.take()
}

#[test]
fn gaps_collapse_only_between_padded_children() {
    assert_eq!(
        gap_bounds(px(2.), Leading::Nothing).child_lefts,
        [8., 32., 56.].map(px)
    );
    assert_eq!(
        gap_bounds(px(0.), Leading::Nothing).child_lefts,
        [0., 16., 32.].map(px)
    );
    assert_eq!(
        gap_bounds(px(2.), Leading::HiddenItem).child_lefts,
        [8., 32., 56.].map(px)
    );
    let with_divider = gap_bounds(px(2.), Leading::Divider);
    assert_eq!(with_divider.child_lefts, [16., 40., 64.].map(px));
    assert_eq!(
        with_divider.divider,
        Some(Bounds::new(point(px(3.5), px(0.)), size(px(1.), px(16.))))
    );
    assert_eq!(
        gap_bounds(px(0.), Leading::Divider).child_lefts,
        [16., 32., 48.].map(px)
    );
}

struct SpacedRow(fn() -> gpui::AnyElement);

impl Render for SpacedRow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(0x202020))
            .text_color(rgb(0xffffff))
            .text_size(px(16.))
            .line_height(px(16.))
            .child((self.0)())
    }
}

fn spaced_row_text(row: fn() -> gpui::AnyElement) -> String {
    spaced_rows(row).remove(0)
}

fn ruled(label: &'static str) -> gpui::Div {
    div()
        .border_l_1()
        .border_color(rgb(0x808080))
        .pl(px(8.))
        .child(label)
}

fn padded(child: impl IntoElement) -> gpui::Div {
    div().flex().px(px(2.)).child(child)
}

#[test]
fn ruled_edges_keep_one_blank_cell_from_text() {
    let row = spaced_row_text(|| {
        div()
            .flex()
            .gap(px(2.))
            .child(padded("alpha.rs"))
            .child("src/")
            .child(padded(ruled("fn alpha")))
            .into_any_element()
    });
    assert_eq!(row, " alpha.rs src/ │ fn alpha");
}

#[test]
fn ruled_edges_next_to_padded_neighbors_do_not_double_the_blank() {
    let row = spaced_row_text(|| {
        div()
            .flex()
            .gap(px(2.))
            .child(padded("main.rs"))
            .child(padded(ruled("fn main")))
            .into_any_element()
    });
    assert_eq!(row, " main.rs │ fn main");
}

#[test]
fn adjacent_ruled_edges_keep_one_blank_cell_between_them() {
    fn chip(label: &'static str) -> gpui::Div {
        div()
            .border_1()
            .border_color(rgb(0x808080))
            .px(px(4.))
            .child(label)
    }
    let row = spaced_row_text(|| {
        div()
            .flex()
            .gap(px(2.))
            .child(chip("a"))
            .child(chip("b"))
            .into_any_element()
    });
    assert_eq!(row, "│ a │ │ b │");
}

#[test]
fn facing_ruled_edges_share_one_blank_cell() {
    let row = spaced_row_text(|| {
        div()
            .flex()
            .child(padded(
                div()
                    .border_r_1()
                    .border_color(rgb(0x808080))
                    .pr(px(8.))
                    .child("a"),
            ))
            .child(padded(ruled("b")))
            .into_any_element()
    });
    assert_eq!(row, " a │ │ b");
}

#[test]
fn toggle_boxes_are_not_ruled_so_their_labels_keep_one_blank_cell() {
    let row = spaced_row_text(|| {
        div()
            .flex()
            .gap(px(2.))
            .child(
                div()
                    .flex()
                    .size(px(16.))
                    .rounded(px(2.))
                    .border_1()
                    .border_color(rgb(0x808080)),
            )
            .child("Trust")
            .into_any_element()
    });
    assert_eq!(row, "☐ Trust");
}

fn square_button(label: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_none()
        .w(px(11.))
        .px(px(2.))
        .justify_center()
        .child(label)
}

fn ruled_row() -> gpui::Div {
    div().flex().border_l_1().border_color(rgb(0x808080))
}

#[test]
fn ruled_edges_keep_their_inner_padding_when_the_edge_child_draws_content() {
    let row = spaced_row_text(|| {
        ruled_row()
            .pl(px(3.))
            .child(square_button("a"))
            .child(square_button("b"))
            .into_any_element()
    });
    assert_eq!(row, "│ a b");

    let row = spaced_row_text(|| {
        div()
            .flex()
            .pl(px(3.))
            .child(square_button("a"))
            .child(square_button("b"))
            .into_any_element()
    });
    assert_eq!(row, "a b");
}

#[test]
fn ruled_edges_drop_their_margin_when_the_neighbor_is_already_blank() {
    let row = spaced_row_text(|| {
        div()
            .flex()
            .gap(px(2.))
            .child(square_button("x"))
            .child(ruled_row().ml(px(5.)).pl(px(5.)).child(square_button("a")))
            .into_any_element()
    });
    assert_eq!(row, "x │ a");

    let row = spaced_row_text(|| {
        div()
            .flex()
            .child("x")
            .child(ruled_row().ml(px(5.)).pl(px(5.)).child("a"))
            .into_any_element()
    });
    assert_eq!(row, "x │ a");

    let row = spaced_row_text(|| {
        div()
            .flex()
            .child(
                div()
                    .flex()
                    .border_r_1()
                    .border_color(rgb(0x808080))
                    .mr(px(5.))
                    .pr(px(5.))
                    .child("a"),
            )
            .child(padded("y"))
            .into_any_element()
    });
    assert_eq!(row, "a │ y");
}

#[test]
fn margins_rounded_up_to_a_cell_drop_beside_an_already_blank_neighbor() {
    let row = spaced_row_text(|| {
        div()
            .flex()
            .child(square_button("x"))
            .child(div().ml(px(5.)).child("1/1"))
            .into_any_element()
    });
    assert_eq!(row, "x 1/1");

    let row = spaced_row_text(|| {
        div()
            .flex()
            .child(div().mr(px(5.)).child("a"))
            .child(div().pl(px(5.)).child("b"))
            .into_any_element()
    });
    assert_eq!(row, "a b");
}

#[test]
fn margins_stay_beside_text_covered_padding_and_whole_cell_margins() {
    let row = spaced_row_text(|| {
        div()
            .flex()
            .child("x")
            .child(div().ml(px(5.)).child("y"))
            .into_any_element()
    });
    assert_eq!(row, "x y");

    let row = spaced_row_text(|| {
        div()
            .flex()
            .child(div().mr(px(5.)).child("a"))
            .child(square_button("b"))
            .into_any_element()
    });
    assert_eq!(row, "a b");

    let row = spaced_row_text(|| {
        div()
            .flex()
            .child(square_button("x"))
            .child(div().ml(px(10.)).child("y"))
            .into_any_element()
    });
    assert_eq!(row, "x  y");
}

#[test]
fn rule_padding_drops_beside_an_already_blank_neighbor_in_block_and_column_flows() {
    fn ruled_on_the_right(label: &'static str) -> gpui::Div {
        div()
            .border_r_1()
            .border_color(rgb(0x808080))
            .pr(px(8.))
            .child(label)
    }
    let row = spaced_row_text(|| {
        div()
            .flex()
            .child(div().px(px(2.)).child(ruled_on_the_right("a")))
            .child(padded("b"))
            .into_any_element()
    });
    assert_eq!(row, " a │ b");

    let row = spaced_row_text(|| {
        div()
            .flex()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .px(px(2.))
                    .child(ruled_on_the_right("a")),
            )
            .child(padded("b"))
            .into_any_element()
    });
    assert_eq!(row, " a │ b");
}

fn spaced_rows(row: fn() -> gpui::AnyElement) -> Vec<String> {
    let grid = view_frame(30, 3, move || SpacedRow(row));
    trimmed_rows(&grid, 0..2)
}

#[derive(Debug, Default, PartialEq)]
struct ActiveWindows {
    platform: Option<AnyWindowHandle>,
    first_is_active: bool,
    second_is_active: bool,
}

#[test]
fn activating_a_window_moves_focus_to_it() {
    let platform = TuiPlatform::new(20, 4);
    let snapshots: Rc<RefCell<Vec<ActiveWindows>>> = Rc::default();
    let handles: Rc<RefCell<Vec<AnyWindowHandle>>> = Rc::default();
    Application::with_platform(platform).run({
        let snapshots = snapshots.clone();
        let handles = handles.clone();
        move |cx: &mut App| {
            let first = cx
                .open_window(WindowOptions::default(), |_, cx| cx.new(|_| Hello))
                .expect("failed to open window");
            let second = cx
                .open_window(WindowOptions::default(), |_, cx| cx.new(|_| Hello))
                .expect("failed to open window");
            handles
                .borrow_mut()
                .extend::<[AnyWindowHandle; 2]>([first.into(), second.into()]);
            cx.spawn(async move |cx| {
                let snapshot = |cx: &mut gpui::AsyncApp| ActiveWindows {
                    platform: cx.update(|cx| cx.active_window()),
                    first_is_active: first
                        .update(cx, |_, window, _| window.is_window_active())
                        .unwrap_or(false),
                    second_is_active: second
                        .update(cx, |_, window, _| window.is_window_active())
                        .unwrap_or(false),
                };
                let settle = Duration::from_millis(20);
                cx.background_executor().timer(settle).await;
                let opened = snapshot(cx);
                first
                    .update(cx, |_, window, _| window.activate_window())
                    .ok();
                cx.background_executor().timer(settle).await;
                let activated = snapshot(cx);
                first.update(cx, |_, window, _| window.remove_window()).ok();
                cx.background_executor().timer(settle).await;
                let closed = snapshot(cx);
                snapshots.borrow_mut().extend([opened, activated, closed]);
                cx.update(|cx| cx.quit());
            })
            .detach();
        }
    });

    let handles = handles.borrow();
    let (first, second) = (Some(handles[0]), Some(handles[1]));
    assert_eq!(
        *snapshots.borrow(),
        [
            ActiveWindows {
                platform: second,
                first_is_active: false,
                second_is_active: true,
            },
            ActiveWindows {
                platform: first,
                first_is_active: true,
                second_is_active: false,
            },
            ActiveWindows {
                platform: second,
                first_is_active: false,
                second_is_active: true,
            },
        ]
    );
}

struct CaretField {
    focus_handle: FocusHandle,
    caret: Option<fn() -> PaintQuad>,
    extra_caret: bool,
}

fn caret_bounds(width: f32) -> Bounds<Pixels> {
    Bounds::new(point(px(24.), px(16.)), size(px(width), px(16.)))
}

fn bar_caret() -> PaintQuad {
    fill(caret_bounds(2.), rgb(0xffffff))
}

fn underline_caret() -> PaintQuad {
    fill(
        Bounds::new(point(px(24.), px(30.)), size(px(8.), px(2.))),
        rgb(0xffffff),
    )
}

fn hollow_caret() -> PaintQuad {
    outline(caret_bounds(8.), rgb(0xffffff), BorderStyle::Solid)
}

impl Render for CaretField {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let focus_handle = self.focus_handle.clone();
        let caret = self.caret;
        let extra_caret = self.extra_caret;
        div()
            .size_full()
            .bg(rgb(0x202020))
            .track_focus(&self.focus_handle)
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, cx| {
                        window.handle_input(
                            &focus_handle,
                            ElementInputHandler::new(bounds, entity),
                            cx,
                        );
                        if let Some(caret) = caret {
                            window.paint_quad(caret());
                        }
                        if extra_caret {
                            window.paint_quad(fill(
                                Bounds::new(point(px(40.), px(16.)), size(px(2.), px(16.))),
                                rgb(0xffffff),
                            ));
                        }
                    },
                )
                .size_full(),
            )
    }
}

impl EntityInputHandler for CaretField {
    fn text_for_range(
        &mut self,
        _range: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        Some(String::new())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        None
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        _text: &str,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        _new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(caret_bounds(8.))
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

fn open_caret_field(
    window: &mut Window,
    cx: &mut App,
    focused: bool,
    caret: Option<fn() -> PaintQuad>,
    extra_caret: bool,
) -> gpui::Entity<CaretField> {
    let field = cx.new(|cx| CaretField {
        focus_handle: cx.focus_handle(),
        caret,
        extra_caret,
    });
    if focused {
        let focus_handle = field.read(cx).focus_handle.clone();
        window.focus(&focus_handle, cx);
    }
    field
}

fn caret_frame(focused: bool, caret: Option<fn() -> PaintQuad>, extra_caret: bool) -> CellGrid {
    first_frame(TuiPlatform::new(8, 3), move |cx: &mut App| {
        cx.open_window(WindowOptions::default(), |window, cx| {
            open_caret_field(window, cx, focused, caret, extra_caret)
        })
        .expect("failed to open window");
    })
}

#[test]
fn the_cursor_follows_the_focused_input_caret() {
    let grid = caret_frame(true, Some(bar_caret), false);
    assert_eq!(grid.cursor, Some(CursorPosition { col: 3, row: 1 }));
    assert_eq!(grid.cursor_shape, CursorShape::Bar);
    assert_eq!(grid.row_text(1), "        ");
    assert_eq!(caret_frame(false, Some(bar_caret), false).cursor, None);
    assert_eq!(caret_frame(true, None, false).cursor, None);
}

#[test]
fn focused_underline_and_hollow_carets_place_the_terminal_cursor_with_their_shape() {
    for (caret, shape) in [
        (underline_caret as fn() -> PaintQuad, CursorShape::Underline),
        (hollow_caret, CursorShape::Block),
    ] {
        let grid = caret_frame(true, Some(caret), false);
        assert_eq!(
            (grid.cursor, grid.cursor_shape),
            (Some(CursorPosition { col: 3, row: 1 }), shape)
        );
        let cell = grid.cell(3, 1).copied().expect("cell");
        assert_eq!(cell.bg, Rgb::new(0x20, 0x20, 0x20), "{shape:?}");
        assert!(!cell.attrs.contains(CellAttrs::UNDERLINE), "{shape:?}");
        assert_eq!(caret_frame(false, Some(caret), false).cursor, None);
    }
}

#[test]
fn extra_carets_in_the_focused_caret_color_become_block_cells() {
    let grid = caret_frame(true, Some(bar_caret), true);
    assert_eq!(grid.cursor, Some(CursorPosition { col: 3, row: 1 }));
    assert_eq!(grid.row_text(1), "        ");
    assert_eq!(
        grid.cell(5, 1).map(|cell| cell.bg),
        Some(Rgb::new(255, 255, 255))
    );
}

struct Label(&'static str);

impl Render for Label {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0)
    }
}

#[test]
fn only_the_active_window_reaches_the_screen() {
    let platform = TuiPlatform::new(20, 4);
    let frames: Rc<RefCell<Vec<String>>> = Rc::default();
    let phases: Rc<RefCell<Vec<Vec<String>>>> = Rc::default();
    platform.set_frame_sink({
        let frames = frames.clone();
        move |grid| {
            frames
                .borrow_mut()
                .push(grid.row_text(0).trim_end().to_string())
        }
    });
    Application::with_platform(platform).run({
        let phases = phases.clone();
        move |cx: &mut App| {
            let first = cx
                .open_window(WindowOptions::default(), |_, cx| cx.new(|_| Label("first")))
                .expect("failed to open window");
            cx.open_window(WindowOptions::default(), |_, cx| {
                cx.new(|_| Label("second"))
            })
            .expect("failed to open window");
            cx.spawn(async move |cx| {
                let settle = Duration::from_millis(50);
                let next_phase = |phases: &Rc<RefCell<Vec<Vec<String>>>>| {
                    phases
                        .borrow_mut()
                        .push(frames.borrow_mut().drain(..).collect())
                };
                cx.background_executor().timer(settle).await;
                next_phase(&phases);
                first
                    .update(cx, |_, window, _| window.activate_window())
                    .ok();
                cx.background_executor().timer(settle).await;
                next_phase(&phases);
                first.update(cx, |_, window, _| window.remove_window()).ok();
                cx.background_executor().timer(settle).await;
                next_phase(&phases);
                cx.update(|cx| cx.quit());
            })
            .detach();
        }
    });

    let phases = phases.borrow();
    let shown = |phase: usize| -> Vec<&str> {
        let mut shown: Vec<&str> = phases[phase].iter().map(String::as_str).collect();
        shown.dedup();
        shown
    };
    assert_eq!(shown(0), ["second"]);
    assert_eq!(shown(1), ["first"]);
    assert_eq!(shown(2), ["second"]);
}

struct ModifierRecorder {
    focus_handle: FocusHandle,
    seen: Rc<RefCell<Vec<Modifiers>>>,
}

impl Render for ModifierRecorder {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let seen = self.seen.clone();
        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_modifiers_changed(move |event, _, _| seen.borrow_mut().push(event.modifiers))
    }
}

fn key_down(keystroke: &str) -> PlatformInput {
    PlatformInput::KeyDown(KeyDownEvent {
        keystroke: Keystroke::parse(keystroke).unwrap(),
        is_held: false,
        prefer_character_input: false,
    })
}

#[test]
fn terminal_keys_release_their_modifiers_after_each_press() {
    let platform = TuiPlatform::new(20, 4);
    let seen: Rc<RefCell<Vec<Modifiers>>> = Rc::default();
    let seen_after_plain_key: Rc<RefCell<Option<usize>>> = Rc::default();
    Application::with_platform(platform.clone()).run({
        let seen = seen.clone();
        let seen_after_plain_key = seen_after_plain_key.clone();
        move |cx: &mut App| {
            cx.open_window(WindowOptions::default(), |window, cx| {
                let recorder = cx.new(|cx| ModifierRecorder {
                    focus_handle: cx.focus_handle(),
                    seen: seen.clone(),
                });
                let focus_handle = recorder.read(cx).focus_handle.clone();
                window.focus(&focus_handle, cx);
                recorder
            })
            .expect("failed to open window");
            cx.spawn(async move |cx| {
                let settle = Duration::from_millis(20);
                cx.background_executor().timer(settle).await;
                platform.handle_input(key_down("a"));
                cx.background_executor().timer(settle).await;
                *seen_after_plain_key.borrow_mut() = Some(seen.borrow().len());
                platform.handle_input(key_down("ctrl-a"));
                cx.background_executor().timer(settle).await;
                cx.update(|cx| cx.quit());
            })
            .detach();
        }
    });

    assert_eq!(*seen_after_plain_key.borrow(), Some(0));
    assert_eq!(*seen.borrow(), [Modifiers::default()]);
}

struct Banner(String);

impl Render for Banner {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

fn floating_frames(
    floating_origin: gpui::Point<Pixels>,
    steps: impl AsyncFnOnce(
        &mut gpui::AsyncApp,
        &Rc<TuiPlatform>,
        gpui::WindowHandle<Banner>,
        gpui::WindowHandle<Banner>,
    ) + 'static,
) -> Vec<CellGrid> {
    let platform = TuiPlatform::new(80, 24);
    let frames: Rc<RefCell<Vec<CellGrid>>> = Rc::default();
    platform.set_frame_sink({
        let frames = frames.clone();
        move |grid| frames.borrow_mut().push(grid)
    });
    Application::with_platform(platform.clone()).run(move |cx: &mut App| {
        let underlying = cx
            .open_window(WindowOptions::default(), |_, cx| {
                cx.new(|_| Banner("BEFORE".into()))
            })
            .expect("failed to open window");
        cx.spawn(async move |cx| {
            let settle = Duration::from_millis(50);
            cx.background_executor().timer(settle).await;
            let floating = cx
                .update(|cx| {
                    cx.open_window(
                        WindowOptions {
                            kind: gpui::WindowKind::Floating,
                            window_bounds: Some(gpui::WindowBounds::Windowed(Bounds::new(
                                floating_origin,
                                size(px(240.), px(48.)),
                            ))),
                            ..Default::default()
                        },
                        |_, cx| cx.new(|_| Banner("FLOATING".into())),
                    )
                })
                .expect("failed to open floating window");
            cx.background_executor().timer(settle).await;
            steps(cx, &platform, underlying, floating).await;
            cx.background_executor().timer(settle).await;
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
    frames.take()
}

#[test]
fn floating_windows_show_underlay_changes_without_redrawing_themselves() {
    let frames = floating_frames(point(px(80.), px(80.)), async |cx, _, underlying, _| {
        underlying
            .update(cx, |banner, _, cx| {
                banner.0 = "AFTER".into();
                cx.notify();
            })
            .ok();
    });
    let last = frames.last().expect("no frame was presented").text();
    assert!(
        last.contains("AFTER") && last.contains("FLOATING"),
        "{last}"
    );
}

#[test]
fn floating_windows_draw_only_a_left_bar() {
    let frames = floating_frames(point(px(80.), px(80.)), async |_, _, _, _| {});
    let last = frames.last().expect("no frame was presented");
    let rows: Vec<String> = (4..9).map(|row| last.row_text(row)).collect();
    assert!(
        rows.iter()
            .all(|row| !row.contains(['┌', '┐', '└', '┘', '─', '│'])),
        "{}",
        last.text()
    );
    for row in 5..8 {
        assert_eq!(
            last.cell(9, row).map(|cell| cell.glyph),
            Some('▌'.into()),
            "{}",
            last.text()
        );
    }
    let content: String = last.row_text(5).chars().skip(10).collect();
    assert!(content.starts_with("FLOATING"), "{}", last.text());
}

#[test]
fn floating_windows_follow_terminal_resizes_that_keep_their_size() {
    let frames = floating_frames(point(px(80.), px(80.)), async |_, platform, _, _| {
        platform.resize(70, 20);
    });
    let last = frames.last().expect("no frame was presented");
    assert_eq!((last.cols, last.rows), (70, 20));
    assert!(last.text().contains("FLOATING"), "{}", last.text());
}

#[test]
fn floating_windows_moved_by_a_resize_are_drawn_at_their_new_origin() {
    let frames = floating_frames(point(px(400.), px(80.)), async |_, platform, _, _| {
        platform.resize(40, 24);
    });
    let last = frames.last().expect("no frame was presented");
    assert_eq!((last.cols, last.rows), (40, 24));
    assert!(last.text().contains("FLOATING"), "{}", last.text());
}

#[test]
fn closing_a_floating_window_shows_the_latest_underlay() {
    let frames = floating_frames(
        point(px(80.), px(80.)),
        async |cx, _, underlying, floating| {
            underlying
                .update(cx, |banner, _, cx| {
                    banner.0 = "AFTER".into();
                    cx.notify();
                })
                .ok();
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
            floating
                .update(cx, |_, window, _| window.remove_window())
                .ok();
        },
    );
    let last = frames.last().expect("no frame was presented").text();
    assert!(
        last.contains("AFTER") && !last.contains("FLOATING"),
        "{last}"
    );
}

#[test]
fn carets_behind_a_floating_window_become_blocks() {
    let platform = TuiPlatform::new(40, 10);
    let frames: Rc<RefCell<Vec<CellGrid>>> = Rc::default();
    platform.set_frame_sink({
        let frames = frames.clone();
        move |grid| frames.borrow_mut().push(grid)
    });
    Application::with_platform(platform).run(move |cx: &mut App| {
        let underlying = cx
            .open_window(WindowOptions::default(), |window, cx| {
                open_caret_field(window, cx, true, Some(bar_caret), true)
            })
            .expect("failed to open window");
        cx.spawn(async move |cx| {
            let settle = Duration::from_millis(50);
            cx.background_executor().timer(settle).await;
            cx.update(|cx| {
                cx.open_window(
                    WindowOptions {
                        kind: gpui::WindowKind::Floating,
                        window_bounds: Some(gpui::WindowBounds::Windowed(Bounds::new(
                            point(px(160.), px(64.)),
                            size(px(120.), px(32.)),
                        ))),
                        ..Default::default()
                    },
                    |_, cx| cx.new(|_| Banner("FLOATING".into())),
                )
            })
            .expect("failed to open floating window");
            cx.background_executor().timer(settle).await;
            underlying.update(cx, |_, _, cx| cx.notify()).ok();
            cx.background_executor().timer(settle).await;
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
    let frames = frames.take();
    let last = frames.last().expect("no frame was presented");
    assert!(last.text().contains("FLOATING"), "{}", last.text());
    assert_eq!(last.cursor, None);
    for col in [3, 5] {
        let cell = last.cell(col, 1).copied().expect("cell");
        assert_eq!(
            (cell.glyph, cell.bg),
            (' '.into(), Rgb::new(255, 255, 255)),
            "col {col}: {}",
            last.text()
        );
    }
}

#[test]
fn combining_and_zwj_text_reach_the_frame() {
    let grid = first_frame(TuiPlatform::new(20, 2), |cx: &mut App| {
        cx.open_window(WindowOptions::default(), |_, cx| {
            cx.new(|_| Banner("cafe\u{301} 👩\u{200d}💻 x".into()))
        })
        .expect("failed to open window");
    });
    assert_eq!(grid.row_text(0).trim_end(), "cafe\u{301} 👩\u{200d}💻 x");
    let zwj = grid.cell(5, 0).expect("cell");
    assert_eq!(zwj.glyph.cells(), 2);
    assert!(
        grid.cell(6, 0)
            .expect("cell")
            .attrs
            .contains(gpui_tui::CellAttrs::WIDE_CONTINUATION)
    );
    assert_eq!(grid.cell(8, 0).expect("cell").glyph, 'x');
}

struct PointerTarget;

impl Render for PointerTarget {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .child(div().id("link").w(px(80.)).h(px(16.)).cursor_pointer())
    }
}

#[test]
fn hovering_a_pointer_element_requests_a_hand() {
    let platform = TuiPlatform::new(20, 4);
    let requested: Rc<RefCell<Vec<gpui::CursorStyle>>> = Rc::default();
    platform.on_cursor_style_change({
        let requested = requested.clone();
        move |style| requested.borrow_mut().push(style)
    });
    Application::with_platform(platform.clone()).run(move |cx: &mut App| {
        cx.open_window(WindowOptions::default(), |_, cx| cx.new(|_| PointerTarget))
            .expect("failed to open window");
        cx.spawn(async move |cx| {
            let settle = Duration::from_millis(30);
            cx.background_executor().timer(settle).await;
            platform.handle_input(PlatformInput::MouseMove(gpui::MouseMoveEvent {
                position: gpui_tui::cell_center(4, 0),
                pressed_button: None,
                modifiers: Modifiers::default(),
            }));
            cx.background_executor().timer(settle).await;
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
    assert_eq!(*requested.borrow(), [gpui::CursorStyle::PointingHand]);
}
