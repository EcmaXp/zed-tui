#![cfg(unix)]

use std::{cell::RefCell, rc::Rc, time::Duration};

use gpui::{
    AnyWindowHandle, App, AppContext as _, Application, Context, IntoElement, ParentElement,
    Render, Styled, Window, WindowOptions, div, px, rgb,
};
use gpui_tui::{CellGrid, Rgb, TuiPlatform};

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
