use gpui::{KeyDownEvent, Keystroke, Modifiers, PlatformInput};

use crate::tui::protocol::{KeyCode, TermEvent};

pub fn translate(event: TermEvent) -> Option<PlatformInput> {
    let TermEvent::Key { code, modifiers } = event;
    key_down(code, modifiers).map(PlatformInput::KeyDown)
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
}
