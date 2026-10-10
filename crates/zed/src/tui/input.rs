use std::time::{Duration, Instant};

use gpui::{
    KeyDownEvent, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PlatformInput, Point, ScrollDelta, ScrollWheelEvent, TouchPhase, point,
};
use gpui_tui::cell_center;

use crate::tui::protocol::{KeyCode, MouseAction, MouseButtonKind, TermEvent};

const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(400);
const SCROLL_LINES: f32 = 3.;

#[derive(Default)]
pub struct InputTranslator {
    last_click: Option<(Instant, u16, u16, MouseButton)>,
    click_count: usize,
    pressed_button: Option<MouseButton>,
}

impl InputTranslator {
    pub fn translate(&mut self, event: TermEvent) -> Vec<PlatformInput> {
        match event {
            TermEvent::Key { code, modifiers } => key_down(code, modifiers)
                .map(PlatformInput::KeyDown)
                .into_iter()
                .collect(),
            TermEvent::Mouse {
                action,
                col,
                row,
                modifiers,
            } => self.mouse(action, col, row, modifiers),
        }
    }

    fn mouse(
        &mut self,
        action: MouseAction,
        col: u16,
        row: u16,
        modifiers: Modifiers,
    ) -> Vec<PlatformInput> {
        let position = cell_center(col, row);
        match action {
            MouseAction::Down(button) => {
                let button = gpui_button(button);
                let now = Instant::now();
                let repeated = self
                    .last_click
                    .is_some_and(|(time, last_col, last_row, last)| {
                        now.duration_since(time) <= DOUBLE_CLICK_INTERVAL
                            && last_col == col
                            && last_row == row
                            && last == button
                    });
                self.click_count = if repeated { self.click_count + 1 } else { 1 };
                self.last_click = Some((now, col, row, button));
                self.pressed_button = Some(button);
                vec![
                    PlatformInput::MouseMove(MouseMoveEvent {
                        position,
                        pressed_button: None,
                        modifiers,
                    }),
                    PlatformInput::MouseDown(MouseDownEvent {
                        button,
                        position,
                        modifiers,
                        click_count: self.click_count,
                        first_mouse: false,
                    }),
                ]
            }
            MouseAction::Up(button) => {
                self.pressed_button = None;
                vec![PlatformInput::MouseUp(MouseUpEvent {
                    button: gpui_button(button),
                    position,
                    modifiers,
                    click_count: self.click_count.max(1),
                })]
            }
            MouseAction::Drag(button) => vec![PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button: Some(gpui_button(button)),
                modifiers,
            })],
            MouseAction::Moved => vec![PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button: self.pressed_button,
                modifiers,
            })],
            MouseAction::ScrollUp => vec![scroll(position, 0., SCROLL_LINES, modifiers)],
            MouseAction::ScrollDown => vec![scroll(position, 0., -SCROLL_LINES, modifiers)],
            MouseAction::ScrollLeft => vec![scroll(position, SCROLL_LINES, 0., modifiers)],
            MouseAction::ScrollRight => vec![scroll(position, -SCROLL_LINES, 0., modifiers)],
        }
    }
}

fn scroll(position: Point<gpui::Pixels>, x: f32, y: f32, modifiers: Modifiers) -> PlatformInput {
    PlatformInput::ScrollWheel(ScrollWheelEvent {
        position,
        delta: ScrollDelta::Lines(point(x, y)),
        modifiers,
        touch_phase: TouchPhase::Moved,
    })
}

fn gpui_button(button: MouseButtonKind) -> MouseButton {
    match button {
        MouseButtonKind::Left => MouseButton::Left,
        MouseButtonKind::Right => MouseButton::Right,
        MouseButtonKind::Middle => MouseButton::Middle,
    }
}

fn key_down(code: KeyCode, mut modifiers: Modifiers) -> Option<KeyDownEvent> {
    let (key, key_char) = match code {
        KeyCode::Char(ch) => {
            if ch.is_control() {
                return None;
            }
            let key = if ch.is_alphabetic() && ch.is_uppercase() {
                modifiers.shift = true;
                ch.to_lowercase().collect::<String>()
            } else if ch == ' ' {
                "space".to_string()
            } else {
                if !ch.is_alphabetic() {
                    modifiers.shift = false;
                }
                ch.to_string()
            };
            let typed = if modifiers.shift && ch.is_alphabetic() {
                ch.to_uppercase().collect::<String>()
            } else {
                ch.to_string()
            };
            let key_char =
                (!modifiers.control && !modifiers.alt && !modifiers.platform).then_some(typed);
            (key, key_char)
        }
        KeyCode::BackTab => {
            modifiers.shift = true;
            ("tab".to_string(), None)
        }
        KeyCode::Function(number) => (format!("f{number}"), None),
        named => {
            let key = match named {
                KeyCode::Enter => "enter",
                KeyCode::Escape => "escape",
                KeyCode::Backspace => "backspace",
                KeyCode::Tab => "tab",
                KeyCode::Left => "left",
                KeyCode::Right => "right",
                KeyCode::Up => "up",
                KeyCode::Down => "down",
                KeyCode::Home => "home",
                KeyCode::End => "end",
                KeyCode::PageUp => "pageup",
                KeyCode::PageDown => "pagedown",
                KeyCode::Insert => "insert",
                KeyCode::Delete => "delete",
                KeyCode::Char(_) | KeyCode::BackTab | KeyCode::Function(_) => return None,
            };
            (key.to_string(), None)
        }
    };
    Some(KeyDownEvent {
        keystroke: Keystroke {
            modifiers,
            key,
            key_char,
        },
        is_held: false,
        prefer_character_input: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::px;

    fn keystroke(code: KeyCode, modifiers: Modifiers) -> Keystroke {
        key_down(code, modifiers).unwrap().keystroke
    }

    #[test]
    fn ctrl_shift_p_matches_the_keymap_notation() {
        let modifiers = Modifiers {
            control: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            keystroke(KeyCode::Char('P'), modifiers).unparse(),
            "ctrl-shift-p"
        );
        assert_eq!(
            keystroke(KeyCode::Char('p'), modifiers).unparse(),
            "ctrl-shift-p"
        );
    }

    #[test]
    fn function_keys_and_alt_letters() {
        assert_eq!(
            keystroke(KeyCode::Function(1), Modifiers::default()).unparse(),
            "f1"
        );
        let alt_x = keystroke(
            KeyCode::Char('x'),
            Modifiers {
                alt: true,
                ..Default::default()
            },
        );
        assert_eq!(alt_x.unparse(), "alt-x");
        assert_eq!(alt_x.key_char, None);
    }

    #[test]
    fn typed_characters_carry_text() {
        let upper = keystroke(KeyCode::Char('A'), Modifiers::default());
        assert_eq!(upper.unparse(), "shift-a");
        assert_eq!(upper.key_char.as_deref(), Some("A"));

        let question = keystroke(
            KeyCode::Char('?'),
            Modifiers {
                shift: true,
                ..Default::default()
            },
        );
        assert_eq!(question.key, "?");
        assert!(!question.modifiers.shift);
        assert_eq!(question.key_char.as_deref(), Some("?"));

        let hangul = keystroke(KeyCode::Char('한'), Modifiers::default());
        assert_eq!(hangul.key_char.as_deref(), Some("한"));
    }

    #[test]
    fn repeated_clicks_increase_click_count() {
        let mut translator = InputTranslator::default();
        let click = |translator: &mut InputTranslator| {
            translator
                .translate(TermEvent::Mouse {
                    action: MouseAction::Down(MouseButtonKind::Left),
                    col: 3,
                    row: 4,
                    modifiers: Modifiers::default(),
                })
                .into_iter()
                .find_map(|input| match input {
                    PlatformInput::MouseDown(event) => Some(event),
                    _ => None,
                })
                .unwrap()
        };
        let first = click(&mut translator);
        let second = click(&mut translator);
        assert_eq!(first.click_count, 1);
        assert_eq!(second.click_count, 2);
        assert_eq!(first.position, point(px(28.), px(72.)));
    }
}
