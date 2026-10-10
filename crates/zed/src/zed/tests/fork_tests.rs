use super::*;
use pretty_assertions::assert_eq;

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

#[cfg(unix)]
#[test]
fn test_tui_multibuffer_breadcrumb_separator_uses_the_full_border_color() {
    struct Breadcrumbs(gpui::Entity<Editor>);

    impl gpui::Render for Breadcrumbs {
        fn render(
            &mut self,
            window: &mut gpui::Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            editor::render_breadcrumb_text(
                vec![language::HighlightedText {
                    text: "fn main".into(),
                    highlights: Vec::new(),
                }],
                None,
                None,
                &self.0,
                true,
                window,
                cx,
            )
        }
    }

    let mut cx = TestAppContext::build_with_text_system(
        gpui::TestDispatcher::new(0),
        None,
        Arc::new(gpui_tui::TuiTextSystem),
    );
    init_test(&mut cx);
    let (_, cx) =
        cx.add_window_view(|window, cx| Breadcrumbs(cx.new(|cx| Editor::single_line(window, cx))));
    cx.run_until_parked();

    let border_color = cx.update(|_, cx| cx.theme().colors().border);
    let mut left_border_colors = cx.update(|window, _| {
        window
            .painted_quads()
            .into_iter()
            .filter(|quad| {
                let widths = quad.border_widths;
                widths.left > gpui::ScaledPixels(0.)
                    && widths.top == gpui::ScaledPixels(0.)
                    && widths.right == gpui::ScaledPixels(0.)
                    && widths.bottom == gpui::ScaledPixels(0.)
            })
            .map(|quad| quad.border_color)
            .collect::<Vec<_>>()
    });
    left_border_colors.dedup();
    assert_eq!(left_border_colors, [border_color]);
}
