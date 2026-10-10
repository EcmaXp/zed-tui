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

#[cfg(unix)]
#[test]
fn test_tui_one_line_diagnostic_takes_one_row() {
    for (editor_width, expected_rows) in [(px(800.), 0), (px(400.), 1)] {
        let dispatcher = gpui::TestDispatcher::new(0);
        let mut cx = TestAppContext::build_with_text_system(
            dispatcher.clone(),
            None,
            Arc::new(gpui_tui::TuiTextSystem),
        );
        let block_heights = gpui::ForegroundExecutor::new(Arc::new(dispatcher))
            .block_test(tui_diagnostic_block_heights(editor_width, &mut cx));
        assert_eq!(
            block_heights,
            [Some(expected_rows)],
            "editor width {editor_width:?}"
        );
    }
}

async fn tui_diagnostic_block_heights(
    editor_width: gpui::Pixels,
    cx: &mut TestAppContext,
) -> Vec<Option<u32>> {
    use language::{
        Diagnostic, DiagnosticEntry, DiagnosticMessage, DiagnosticSeverity, DiagnosticSourceKind,
        LanguageServerId, PointUtf16, Unclipped,
    };

    let app_state = init_test(cx);
    cx.update(|cx| {
        diagnostics::init(cx);
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(
                    r#"{
                        "ui_font_size": 10,
                        "buffer_font_size": 16,
                        "buffer_line_height": { "custom": 1.0 }
                    }"#,
                    cx,
                )
                .result()
                .unwrap();
        });
    });
    app_state
        .fs
        .as_fake()
        .insert_tree(
            path!("/root"),
            json!({ "main.rs": "fn main() {\n    let value: u32 = \"text\";\n}\n" }),
        )
        .await;
    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/root/main.rs"), cx)
        })
        .await
        .unwrap();
    project.update(cx, |project, cx| {
        project.lsp_store().update(cx, |lsp_store, cx| {
            lsp_store
                .update_diagnostic_entries(
                    LanguageServerId(0),
                    PathBuf::from(path!("/root/main.rs")),
                    None,
                    None,
                    vec![DiagnosticEntry::new(
                        Unclipped(PointUtf16::new(1, 21))..Unclipped(PointUtf16::new(1, 27)),
                        Diagnostic {
                            severity: DiagnosticSeverity::ERROR,
                            message: DiagnosticMessage::from("expected u32, found &str"),
                            source_kind: DiagnosticSourceKind::Pushed,
                            is_primary: true,
                            ..Default::default()
                        },
                    )],
                    cx,
                )
                .unwrap();
        });
    });

    let (editor, cx) =
        cx.add_window_view(|window, cx| Editor::for_buffer(buffer, Some(project), window, cx));
    cx.run_until_parked();
    editor.update_in(cx, |editor, window, cx| {
        editor.go_to_diagnostic(&editor::actions::GoToDiagnostic::default(), window, cx);
    });
    cx.run_until_parked();

    let draw_size = gpui::size(editor_width, px(400.));
    cx.simulate_resize(draw_size);
    for _ in 0..2 {
        cx.draw(gpui::Point::default(), draw_size, |_, _| {
            editor.clone().into_any_element()
        });
        cx.run_until_parked();
    }
    editor.update_in(cx, |editor, window, cx| {
        let snapshot = editor.snapshot(window, cx);
        snapshot
            .blocks_in_range(DisplayRow(0)..DisplayRow(snapshot.max_point().row().0 + 1))
            .filter_map(|(_, block)| match block {
                editor::display_map::Block::Custom(block) => Some(block.height),
                _ => None,
            })
            .collect()
    })
}
