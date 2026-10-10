#![cfg(unix)]

use std::{cell::RefCell, rc::Rc, time::Duration};

use gpui::{
    Application, KeybindingKeystroke, Keystroke, Modifiers, MouseMoveEvent, PlatformInput,
    WindowOptions,
};
use gpui_tui::{CellGrid, TuiPlatform};
use settings::SettingsStore;
use ui::{KeyBinding, KeybindingHint, TintColor, prelude::*};

fn keybinding(source: &str) -> KeyBinding {
    let keystrokes = source
        .split_whitespace()
        .map(|keystroke| KeybindingKeystroke::from_keystroke(Keystroke::parse(keystroke).unwrap()))
        .collect::<Vec<_>>();
    KeyBinding::from_keystrokes(keystrokes.into(), false)
}

struct Rows(Vec<Box<dyn Fn(&App) -> AnyElement>>);

impl Render for Rows {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().colors().background)
            .children(self.0.iter().map(|row| row(cx)))
    }
}

fn first_frame(cols: u16, rows: Rows) -> CellGrid {
    let platform = TuiPlatform::new(cols, 4);
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
    Application::with_platform(platform).run(move |cx: &mut App| {
        let settings_store = settings::SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        cx.open_window(WindowOptions::default(), |_, cx| cx.new(|_| rows))
            .expect("failed to open window");
    });
    let mut frames = frames.borrow_mut();
    assert!(!frames.is_empty(), "no frame was presented");
    frames.remove(0)
}

#[test]
fn keybinding_hint_keys_sit_inline_on_a_cell_grid() {
    let grid = first_frame(
        40,
        Rows(vec![Box::new(|cx| {
            KeybindingHint::new(
                keybinding("ctrl-shift-e"),
                cx.theme().colors().surface_background,
            )
            .suffix("Focus Content")
            .into_any_element()
        })]),
    );
    assert_eq!(
        grid.row_text(0).trim_end(),
        " Ctrl-Shift-E Focus Content",
        "{}",
        grid.text()
    );
}

#[test]
fn the_platform_modifier_is_labeled_for_the_host_keyboard() {
    let grid = first_frame(
        40,
        Rows(vec![Box::new(|_| keybinding("cmd-c").into_any_element())]),
    );
    let expected = if cfg!(target_os = "macos") {
        "Cmd-C"
    } else {
        "Super-C"
    };
    assert_eq!(grid.row_text(0).trim(), expected, "{}", grid.text());
}

const ONE_DARK_TINTS: &str = r##"{
    "experimental.theme_overrides": {
        "elevated_surface.background": "#2f343eff",
        "info.background": "#74ade81a",
        "text.accent": "#74ade8ff"
    }
}"##;

struct TintedButtonOnSurface;

impl Render for TintedButtonOnSurface {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().colors().elevated_surface_background)
            .child(div().h(px(16.)))
            .child(
                Button::new("configure", "Configure")
                    .full_width()
                    .style(ButtonStyle::Tinted(TintColor::Accent)),
            )
    }
}

fn frames_around_a_hover(cols: u16, rows: u16, row: u16) -> (CellGrid, CellGrid) {
    let platform = TuiPlatform::new(cols, rows);
    let frames: Rc<RefCell<Vec<CellGrid>>> = Rc::default();
    let frames_before_hover: Rc<RefCell<usize>> = Rc::default();
    platform.set_frame_sink({
        let frames = frames.clone();
        move |grid| frames.borrow_mut().push(grid)
    });
    Application::with_platform(platform.clone()).run({
        let frames = frames.clone();
        let frames_before_hover = frames_before_hover.clone();
        move |cx: &mut App| {
            let mut settings_store = SettingsStore::test(cx);
            settings_store
                .set_user_settings(ONE_DARK_TINTS, cx)
                .expect("the theme overrides parse");
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            cx.open_window(WindowOptions::default(), |_, cx| {
                cx.new(|_| TintedButtonOnSurface)
            })
            .expect("failed to open window");
            cx.spawn(async move |cx| {
                let settle = Duration::from_millis(50);
                cx.background_executor().timer(settle).await;
                *frames_before_hover.borrow_mut() = frames.borrow().len();
                platform.handle_input(PlatformInput::MouseMove(MouseMoveEvent {
                    position: gpui_tui::cell_center(cols / 2, row),
                    pressed_button: None,
                    modifiers: Modifiers::default(),
                }));
                cx.background_executor().timer(settle).await;
                cx.update(|cx| cx.quit());
            })
            .detach();
        }
    });
    let frames = frames.take();
    let before_hover = *frames_before_hover.borrow();
    let resting = frames
        .get(before_hover.saturating_sub(1))
        .expect("no frame before the hover")
        .clone();
    let hovered = frames.last().expect("no frame after the hover").clone();
    (resting, hovered)
}

#[test]
fn hovered_tinted_buttons_stand_out_from_the_surface_on_a_cell_grid() {
    let (resting, hovered) = frames_around_a_hover(30, 4, 1);
    assert!(
        hovered.row_text(1).contains("Configure"),
        "{}",
        hovered.text()
    );
    let background = |grid: &CellGrid, row: i32| grid.cell(2, row).expect("cell").bg;
    let surface = background(&hovered, 0);
    let resting_button = background(&resting, 1);
    let hovered_button = background(&hovered, 1);
    assert!(resting_button.distance(surface) > 0);
    assert!(
        hovered_button.distance(surface) > resting_button.distance(surface),
        "hovered {hovered_button:?}, resting {resting_button:?}, surface {surface:?}"
    );
    assert!(
        hovered_button.distance(resting_button) >= 24,
        "hovered {hovered_button:?}, resting {resting_button:?}"
    );
}
