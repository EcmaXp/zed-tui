#![cfg(unix)]

use gpui::{KeyContext, Keystroke, Modifiers};
use gpui_tui::TuiPlatform;

#[test]
fn tui_platform_makes_ctrl_the_secondary_modifier() {
    let _platform = TuiPlatform::new(20, 4);
    assert!(!gpui::uses_mac_key_conventions());

    assert_eq!(Modifiers::secondary_key(), Modifiers::control());
    assert!(Modifiers::control().secondary());
    assert!(!Modifiers::command().secondary());

    let secondary_a = Keystroke::parse("secondary-a").unwrap();
    assert_eq!(secondary_a.modifiers, Modifiers::control());
}

#[test]
fn tui_platform_reports_the_os_the_linux_way() {
    let _platform = TuiPlatform::new(20, 4);
    assert_eq!(
        KeyContext::new_with_defaults()
            .get("os")
            .map(|os| os.as_ref()),
        Some("linux")
    );
}

#[test]
fn keyboard_layout_name_is_the_tui_keymap_context() {
    let platform = TuiPlatform::new(20, 4);
    assert_eq!(
        gpui::Platform::keyboard_layout(&*platform).name(),
        "Terminal"
    );
}
