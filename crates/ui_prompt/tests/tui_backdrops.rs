#![cfg(unix)]

use std::{cell::RefCell, rc::Rc, time::Duration};

use gpui::{
    App, AppContext as _, Application, Context, DismissEvent, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement as _, PromptLevel, Render, Styled as _, Window, WindowOptions, div,
    px,
};
use gpui_tui::{CellGrid, Rgb, TuiPlatform};
use settings::SettingsStore;
use workspace::{ModalLayer, ModalView};

const COLS: u16 = 60;
const ROWS: u16 = 16;
const IN_WINDOW_PROMPTS: &str = r#"{ "use_system_prompts": false }"#;

fn last_frame(open: impl FnOnce(&mut App) -> Box<dyn std::any::Any> + 'static) -> CellGrid {
    let platform = TuiPlatform::new(COLS, ROWS);
    platform.set_canvas(Rgb::new(40, 44, 51));
    let frames: Rc<RefCell<Vec<CellGrid>>> = Rc::default();
    platform.set_frame_sink({
        let frames = frames.clone();
        move |grid| frames.borrow_mut().push(grid)
    });
    Application::with_platform(platform).run(move |cx: &mut App| {
        let mut settings_store = SettingsStore::test(cx);
        settings_store
            .set_user_settings(IN_WINDOW_PROMPTS, cx)
            .expect("the prompt settings parse");
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        ui_prompt::init(cx);
        let kept_alive = open(cx);
        cx.spawn(async move |cx| {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            drop(kept_alive);
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
    frames.take().pop().expect("no frame was presented")
}

fn assert_corners_keep_the_terminal_background(grid: &CellGrid) {
    for (col, row) in [(0, 0), (i32::from(COLS) - 1, i32::from(ROWS) - 1)] {
        assert_eq!(
            grid.cell(col, row).map(|cell| cell.bg),
            Some(Rgb::default()),
            "cell {col},{row}\n{}",
            grid.text()
        );
    }
}

struct Blank;

impl Render for Blank {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full()
    }
}

#[test]
fn prompts_on_a_cell_grid_leave_unpainted_cells_at_the_terminal_background() {
    let grid = last_frame(|cx| {
        let window = cx
            .open_window(WindowOptions::default(), |_, cx| cx.new(|_| Blank))
            .expect("failed to open window");
        let answer = window
            .update(cx, |_, window, cx| {
                window.prompt(
                    PromptLevel::Info,
                    "Save changes?",
                    None,
                    &["Save", "Cancel"],
                    cx,
                )
            })
            .expect("failed to open the prompt");
        Box::new(answer)
    });
    assert!(grid.text().contains("Save changes?"), "{}", grid.text());
    assert_corners_keep_the_terminal_background(&grid);
}

struct FadingModal {
    focus_handle: FocusHandle,
}

impl Render for FadingModal {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().w(px(160.)).h(px(32.)).child("Trust this folder?")
    }
}

impl Focusable for FadingModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for FadingModal {}

impl ModalView for FadingModal {
    fn fade_out_background(&self) -> bool {
        true
    }
}

#[test]
fn faded_modal_backgrounds_on_a_cell_grid_leave_unpainted_cells_at_the_terminal_background() {
    let grid = last_frame(|cx| {
        let window = cx
            .open_window(WindowOptions::default(), |window, cx| {
                cx.new(|cx| {
                    let mut modal_layer = ModalLayer::new();
                    modal_layer.toggle_modal(window, cx, |_, cx| FadingModal {
                        focus_handle: cx.focus_handle(),
                    });
                    modal_layer
                })
            })
            .expect("failed to open window");
        Box::new(window)
    });
    assert!(
        grid.text().contains("Trust this folder?"),
        "{}",
        grid.text()
    );
    assert_corners_keep_the_terminal_background(&grid);
}
