use std::time::{Duration, Instant};

use gpui::{
    KeyDownEvent, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PlatformInput, Point, ScrollDelta, ScrollWheelEvent, TouchPhase, point,
};
use gpui_tui::cell_center;

use crate::tui::protocol::{KeyCode, MouseAction, MouseButtonKind, TermEvent};

const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(400);
const SCROLL_LINES: f32 = 3.;

pub enum Translated {
    Input(PlatformInput),
    Text(String),
}

#[derive(Default)]
pub struct InputTranslator {
    last_click: Option<(Instant, u16, u16, MouseButton)>,
    click_count: usize,
    pressed_button: Option<MouseButton>,
    last_hover: Option<Hover>,
}

#[derive(Clone, Copy, PartialEq)]
struct Hover {
    col: u16,
    row: u16,
    pressed_button: Option<MouseButton>,
    modifiers: Modifiers,
}

impl InputTranslator {
    pub fn translate(&mut self, event: TermEvent) -> Vec<Translated> {
        match event {
            TermEvent::Key { code, modifiers } => key_down(code, modifiers)
                .map(|event| vec![Translated::Input(PlatformInput::KeyDown(event))])
                .unwrap_or_default(),
            TermEvent::Paste(text) => vec![Translated::Text(text)],
            TermEvent::Mouse {
                action,
                col,
                row,
                modifiers,
            } => self
                .mouse(action, col, row, modifiers)
                .into_iter()
                .map(Translated::Input)
                .collect(),
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
                self.last_hover = Some(Hover {
                    col,
                    row,
                    pressed_button: Some(button),
                    modifiers,
                });
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
                self.last_hover = Some(Hover {
                    col,
                    row,
                    pressed_button: None,
                    modifiers,
                });
                vec![PlatformInput::MouseUp(MouseUpEvent {
                    button: gpui_button(button),
                    position,
                    modifiers,
                    click_count: self.click_count.max(1),
                })]
            }
            MouseAction::Drag(button) => self.hover(Hover {
                col,
                row,
                pressed_button: Some(gpui_button(button)),
                modifiers,
            }),
            MouseAction::Moved => self.hover(Hover {
                col,
                row,
                pressed_button: self.pressed_button,
                modifiers,
            }),
            MouseAction::ScrollUp => vec![scroll(position, 0., SCROLL_LINES, modifiers)],
            MouseAction::ScrollDown => vec![scroll(position, 0., -SCROLL_LINES, modifiers)],
            MouseAction::ScrollLeft => vec![scroll(position, SCROLL_LINES, 0., modifiers)],
            MouseAction::ScrollRight => vec![scroll(position, -SCROLL_LINES, 0., modifiers)],
        }
    }

    fn hover(&mut self, hover: Hover) -> Vec<PlatformInput> {
        if self.last_hover.replace(hover) == Some(hover) {
            return Vec::new();
        }
        vec![PlatformInput::MouseMove(MouseMoveEvent {
            position: cell_center(hover.col, hover.row),
            pressed_button: hover.pressed_button,
            modifiers: hover.modifiers,
        })]
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
    fn pastes_become_text() {
        let mut translator = InputTranslator::default();
        let translated = translator.translate(TermEvent::Paste("넓은 글자".into()));
        assert!(matches!(translated.as_slice(), [Translated::Text(text)] if text == "넓은 글자"));
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
                .find_map(|translated| match translated {
                    Translated::Input(PlatformInput::MouseDown(event)) => Some(event),
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

    fn mouse_moves(
        translator: &mut InputTranslator,
        action: MouseAction,
        col: u16,
        shift: bool,
    ) -> usize {
        translator
            .translate(TermEvent::Mouse {
                action,
                col,
                row: 2,
                modifiers: Modifiers {
                    shift,
                    ..Default::default()
                },
            })
            .iter()
            .filter(|translated| {
                matches!(translated, Translated::Input(PlatformInput::MouseMove(_)))
            })
            .count()
    }

    #[test]
    fn hovering_within_one_cell_moves_once() {
        let mut translator = InputTranslator::default();
        let left = MouseButtonKind::Left;
        assert_eq!(
            mouse_moves(&mut translator, MouseAction::Moved, 5, false),
            1
        );
        assert_eq!(
            mouse_moves(&mut translator, MouseAction::Moved, 5, false),
            0
        );
        assert_eq!(
            mouse_moves(&mut translator, MouseAction::Moved, 6, false),
            1
        );
        assert_eq!(mouse_moves(&mut translator, MouseAction::Moved, 6, true), 1);
        assert_eq!(
            mouse_moves(&mut translator, MouseAction::Down(left), 6, true),
            1
        );
        assert_eq!(
            mouse_moves(&mut translator, MouseAction::Drag(left), 6, true),
            0
        );
        assert_eq!(
            mouse_moves(&mut translator, MouseAction::Drag(left), 7, true),
            1
        );
        assert_eq!(
            mouse_moves(&mut translator, MouseAction::Up(left), 7, true),
            0
        );
        assert_eq!(mouse_moves(&mut translator, MouseAction::Moved, 7, true), 0);
        assert_eq!(
            mouse_moves(&mut translator, MouseAction::Moved, 7, false),
            1
        );
    }
}
