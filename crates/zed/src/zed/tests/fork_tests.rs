use super::*;

#[cfg(unix)]
#[test]
fn test_tui_drops_font_size_keybindings() {
    let mut cx = TestAppContext::build_with_text_system(
        gpui::TestDispatcher::new(0),
        None,
        Arc::new(gpui_tui::TuiTextSystem),
    );
    init_keymap_test(&mut cx);

    let user_binding = KeyBinding::new(
        "ctrl-shift-=",
        zed_actions::IncreaseBufferFontSize { persist: true },
        None,
    );
    cx.update(|cx| reload_keymaps(cx, vec![user_binding]));
    cx.update(|cx| {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        if let Some(binding) = keymap.bindings().find(|b| is_font_size_keybinding(b)) {
            panic!(
                "expected no font size bindings in the terminal UI, but found `{}`",
                binding.action().name()
            );
        }
        assert!(
            keymap
                .bindings()
                .any(|binding| binding.action().name() == "editor::FoldAll"),
            "expected the fold-all chord to survive the font size filter"
        );
    });
    cx.update(|cx| cx.quit());
    cx.run_until_parked();
}
