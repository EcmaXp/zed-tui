#![cfg(unix)]

use std::{cell::RefCell, rc::Rc};

use gpui::{Application, KeybindingKeystroke, Keystroke, WindowOptions};
use gpui_tui::{CellGrid, TuiPlatform};
use ui::{KeyBinding, prelude::*};

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
