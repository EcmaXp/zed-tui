use super::*;
use pretty_assertions::assert_eq;

fn count_notifications<T: 'static>(
    entity: &Entity<T>,
    cx: &mut TestAppContext,
) -> (std::rc::Rc<std::cell::Cell<usize>>, gpui::Subscription) {
    let count = std::rc::Rc::new(std::cell::Cell::new(0));
    let subscription = cx.update(|cx| {
        let count = count.clone();
        cx.observe(entity, move |_, _| count.set(count.get() + 1))
    });
    (count, subscription)
}

#[gpui::test]
async fn test_status_items_notify_only_when_their_state_changes(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    app_state
        .fs
        .as_fake()
        .insert_tree(path!("/root"), json!({"a.txt": "one\ntwo\n"}))
        .await;
    cx.update(|cx| {
        open_paths(
            &[PathBuf::from(path!("/root/a.txt"))],
            app_state.clone(),
            workspace::OpenOptions::default(),
            cx,
        )
    })
    .await
    .unwrap();
    cx.run_until_parked();

    let window = cx.update(|cx| cx.windows()[0].downcast::<MultiWorkspace>().unwrap());
    let (editor, status_bar) = window
        .read_with(cx, |multi_workspace, cx| {
            let workspace = multi_workspace.workspace().read(cx);
            (
                workspace
                    .active_item(cx)
                    .unwrap()
                    .downcast::<Editor>()
                    .unwrap(),
                workspace.status_bar().clone(),
            )
        })
        .unwrap();
    let buffer = cx.read(|cx| editor.read(cx).active_buffer(cx).unwrap());
    let (line_ending, language, encoding, edit_prediction) = cx.read(|cx| {
        let status_bar = status_bar.read(cx);
        (
            status_bar
                .item_of_type::<line_ending_selector::LineEndingIndicator>()
                .unwrap(),
            status_bar
                .item_of_type::<language_selector::ActiveBufferLanguage>()
                .unwrap(),
            status_bar
                .item_of_type::<encoding_selector::ActiveBufferEncoding>()
                .unwrap(),
            status_bar
                .item_of_type::<edit_prediction_ui::EditPredictionButton>()
                .unwrap(),
        )
    });
    let notify_editor = |cx: &mut TestAppContext| {
        editor.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
    };

    window
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| editor.insert("x", window, cx))
        })
        .unwrap();
    cx.run_until_parked();

    let (line_ending_count, _line_ending_subscription) = count_notifications(&line_ending, cx);
    let (language_count, _language_subscription) = count_notifications(&language, cx);
    let (encoding_count, _encoding_subscription) = count_notifications(&encoding, cx);
    let (edit_prediction_count, _edit_prediction_subscription) =
        count_notifications(&edit_prediction, cx);
    let counts = || {
        [
            line_ending_count.get(),
            language_count.get(),
            encoding_count.get(),
            edit_prediction_count.get(),
        ]
    };
    let changes_since = |before: [usize; 4]| {
        let after = counts();
        [0, 1, 2, 3].map(|index| after[index] - before[index])
    };

    let before = counts();
    window
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.insert("abc", window, cx);
                editor.move_left(&editor::actions::MoveLeft, window, cx);
                editor.insert("y", window, cx);
            })
        })
        .unwrap();
    notify_editor(cx);
    assert_eq!(
        changes_since(before),
        [0, 0, 0, 0],
        "edits and cursor moves that leave an item's state unchanged must not notify it"
    );

    let before = counts();
    buffer.update(cx, |buffer, cx| while buffer.undo(cx).is_some() {});
    notify_editor(cx);
    assert!(!cx.read(|cx| buffer.read(cx).is_dirty()));
    let [
        line_ending_changes,
        language_changes,
        encoding_changes,
        edit_prediction_changes,
    ] = changes_since(before);
    assert_eq!(
        [
            line_ending_changes,
            edit_prediction_changes,
            encoding_changes
        ],
        [language_changes, language_changes, language_changes + 1],
        "only the encoding item, which shows the dirty state, adds its own notification"
    );

    let before = counts();
    buffer.update(cx, |buffer, cx| {
        buffer.set_line_ending(language::LineEnding::Windows, cx)
    });
    notify_editor(cx);
    let [
        line_ending_changes,
        language_changes,
        encoding_changes,
        edit_prediction_changes,
    ] = changes_since(before);
    assert_eq!(
        [
            line_ending_changes,
            edit_prediction_changes,
            encoding_changes
        ],
        [language_changes + 1, language_changes, language_changes],
        "only the line ending item adds its own notification"
    );

    let before = counts();
    buffer.update(cx, |buffer, cx| buffer.set_language(Some(rust_lang()), cx));
    notify_editor(cx);
    let [
        line_ending_changes,
        language_changes,
        encoding_changes,
        edit_prediction_changes,
    ] = changes_since(before);
    assert_eq!(
        [language_changes, edit_prediction_changes, encoding_changes],
        [
            line_ending_changes + 1,
            line_ending_changes + 1,
            line_ending_changes
        ],
        "the language item and the edit prediction button both show the language"
    );
}

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
