#![cfg(unix)]

use std::{cell::RefCell, ops::Range, rc::Rc, time::Duration};

use gpui::{
    AnyWindowHandle, App, AppContext as _, Application, BorderStyle, Bounds, Context,
    ElementInputHandler, EntityInputHandler, FocusHandle, InteractiveElement as _, IntoElement,
    KeyDownEvent, Keystroke, Modifiers, PaintQuad, ParentElement, Pixels, PlatformInput, Render,
    Styled, UTF16Selection, Window, WindowOptions, canvas, div, fill, outline, point, px, rgb,
    size,
};
use gpui_tui::{CellAttrs, CellGrid, CursorPosition, CursorShape, Rgb, TuiPlatform};

struct Hello;

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
