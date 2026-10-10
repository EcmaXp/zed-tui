use super::*;
use pretty_assertions::assert_eq;

#[gpui::test]
async fn test_highlight_background_without_ranges_before_or_after_does_not_notify(
    cx: &mut TestAppContext,
) {
    fn highlight_read_ranges(
        editor: &mut Editor,
        ranges: &[Range<Anchor>],
        cx: &mut Context<Editor>,
    ) {
        editor.highlight_background(
            HighlightKey::DocumentHighlightRead,
            ranges,
            |_, theme| theme.colors().editor_document_highlight_read_background,
            cx,
        );
    }

    init_test(cx, |_| {});
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state("fn mainˇ() {}");
    cx.run_until_parked();

    let notifications = Arc::new(AtomicUsize::new(0));
    let editor = cx.editor.clone();
    let _subscription = cx.update(|_, cx| {
        let notifications = notifications.clone();
        cx.observe(&editor, move |_, _| {
            notifications.fetch_add(1, atomic::Ordering::SeqCst);
        })
    });

    cx.update_editor(|editor, _, cx| highlight_read_ranges(editor, &[], cx));
    cx.update_editor(|editor, _, cx| highlight_read_ranges(editor, &[], cx));
    cx.run_until_parked();
    assert_eq!(
        notifications.load(atomic::Ordering::SeqCst),
        0,
        "expected no notification when the key had no ranges and still has none",
    );

    let range = cx.update_editor(|editor, _, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        snapshot.anchor_before(Point::new(0, 3))..snapshot.anchor_after(Point::new(0, 7))
    });
    cx.update_editor(|editor, _, cx| highlight_read_ranges(editor, &[range], cx));
    cx.run_until_parked();
    let notifications_after_adding = notifications.load(atomic::Ordering::SeqCst);
    assert!(
        notifications_after_adding > 0,
        "expected a notification when ranges are added",
    );

    cx.update_editor(|editor, _, cx| highlight_read_ranges(editor, &[], cx));
    cx.run_until_parked();
    assert!(
        notifications.load(atomic::Ordering::SeqCst) > notifications_after_adding,
        "expected a notification when ranges are removed",
    );
}

#[gpui::test]
async fn test_refreshing_document_highlights_without_any_before_or_after_does_not_notify(
    cx: &mut TestAppContext,
) {
    init_test(cx, |_| {});
    update_test_editor_settings(cx, &|settings| {
        settings.cursor_blink = Some(false);
    });

    let mut cx = EditorLspTestContext::new_rust(
        lsp::ServerCapabilities {
            document_highlight_provider: Some(lsp::OneOf::Left(true)),
            ..lsp::ServerCapabilities::default()
        },
        cx,
    )
    .await;
    let debounce = Duration::from_millis(
        cx.update(|_, cx| EditorSettings::get_global(cx).lsp_highlight_debounce.0),
    );
    let request_count = Arc::new(AtomicUsize::new(0));
    let _request_handler =
        cx.set_request_handler::<lsp::request::DocumentHighlightRequest, _, _>({
            let request_count = request_count.clone();
            move |_, _, _| {
                request_count.fetch_add(1, atomic::Ordering::SeqCst);
                async move { Ok(Some(Vec::new())) }
            }
        });
    cx.set_state(indoc! {"
        fn main() {
            let foo = 1;
            fˇoo;
        }
    "});
    cx.executor()
        .advance_clock(CODE_ACTIONS_DEBOUNCE_TIMEOUT * 4);
    cx.run_until_parked();
    let requests_before_refresh = request_count.load(atomic::Ordering::SeqCst);

    let notifications = Arc::new(AtomicUsize::new(0));
    let editor = cx.editor.clone();
    let _subscription = cx.update(|_, cx| {
        let notifications = notifications.clone();
        cx.observe(&editor, move |_, _| {
            notifications.fetch_add(1, atomic::Ordering::SeqCst);
        })
    });
    cx.update_editor(|editor, _, cx| {
        editor.refresh_document_highlights(cx);
    });
    cx.executor().advance_clock(debounce);
    cx.run_until_parked();

    assert_eq!(
        request_count.load(atomic::Ordering::SeqCst),
        requests_before_refresh + 1,
        "expected the refresh to query the server",
    );
    assert_eq!(document_highlight_count(&mut cx), 0);
    assert_eq!(
        notifications.load(atomic::Ordering::SeqCst),
        0,
        "expected an empty response to not notify when there were no document highlights before",
    );
}

#[gpui::test]
async fn test_indent_guides_stop_at_excerpt_boundaries(cx: &mut TestAppContext) {
    init_test(cx, |_| {});

    let buffer_a = cx.new(|cx| {
        Buffer::local(
            indoc! {"
                fn first() {
                    if a {
                        one();
                        two();
                    }
                    if b {
                        three();
                        four();
                    }
                }
            "},
            cx,
        )
    });
    let buffer_b = cx.new(|cx| {
        Buffer::local(
            indoc! {"
                fn second() {
                    if c {
                        five();
                    }
                }
            "},
            cx,
        )
    });
    let multi_buffer = cx.new(|cx| {
        let mut multi_buffer = MultiBuffer::new(ReadWrite);
        multi_buffer.set_excerpts_for_path(
            PathKey::sorted(0),
            buffer_a.clone(),
            [
                Point::new(2, 0)..Point::new(3, 0),
                Point::new(6, 0)..Point::new(7, 0),
            ],
            0,
            cx,
        );
        multi_buffer.set_excerpts_for_path(
            PathKey::sorted(1),
            buffer_b.clone(),
            [Point::new(2, 0)..Point::new(2, 0)],
            0,
            cx,
        );
        multi_buffer
    });
    let (editor, cx) = cx.add_window_view(|window, cx| {
        Editor::new(EditorMode::full(), multi_buffer, None, window, cx)
    });

    let (text, guides) = editor.update_in(cx, |editor, window, cx| {
        let snapshot = editor.snapshot(window, cx).display_snapshot;
        let max_row = snapshot.buffer_snapshot().max_point().row;
        let mut guides = crate::indent_guides::indent_guides_in_range(
            editor,
            MultiBufferRow(0)..MultiBufferRow(max_row),
            true,
            &snapshot,
            cx,
        )
        .into_iter()
        .map(|guide| {
            (
                guide.buffer_id,
                guide.start_row.0..=guide.end_row.0,
                guide.depth,
            )
        })
        .collect::<Vec<_>>();
        guides.sort_by_key(|(_, rows, depth)| (*rows.start(), *depth));
        (snapshot.buffer_snapshot().text(), guides)
    });

    assert_eq!(
        text,
        "        one();\n        two();\n        three();\n        four();\n        five();"
    );
    let buffer_a = buffer_a.read_with(cx, |buffer, _| buffer.remote_id());
    let buffer_b = buffer_b.read_with(cx, |buffer, _| buffer.remote_id());
    assert_eq!(
        guides,
        vec![
            (buffer_a, 0..=1, 0),
            (buffer_a, 0..=1, 1),
            (buffer_a, 2..=3, 0),
            (buffer_a, 2..=3, 1),
            (buffer_b, 4..=4, 0),
            (buffer_b, 4..=4, 1),
        ]
    );
}

#[gpui::test]
async fn test_edit_keys_on_a_folded_buffer_only_unfold_it(cx: &mut TestAppContext) {
    init_test(cx, |_| {});
    let untouched = "a0\nb0\nc0\na1\nb1\nc1";
    let cases: [(Box<dyn Action>, &str, &str); 5] = [
        (Box::new(Newline), untouched, "\na0\nb0\nc0\na1\nb1\nc1"),
        (Box::new(Tab), untouched, "    a0\nb0\nc0\na1\nb1\nc1"),
        (Box::new(Delete), untouched, "0\nb0\nc0\na1\nb1\nc1"),
        (Box::new(DeleteLine), untouched, "b0\nc0\na1\nb1\nc1"),
        (
            Box::new(HandleInput("x".to_string())),
            "xa0\nb0\nc0\na1\nb1\nc1",
            "xxa0\nb0\nc0\na1\nb1\nc1",
        ),
    ];
    for (action, after_first_press, after_second_press) in cases {
        let (editor, cx) = cx.add_window_view(|window, cx| {
            let multi_buffer = MultiBuffer::build_multi(
                [
                    ("a0\nb0\nc0", vec![Point::row_range(0..3)]),
                    ("a1\nb1\nc1", vec![Point::row_range(0..3)]),
                ],
                cx,
            );
            let buffer_ids = multi_buffer
                .read(cx)
                .snapshot(cx)
                .excerpts()
                .map(|excerpt| excerpt.context.start.buffer_id)
                .collect::<Vec<_>>();
            let mut editor = Editor::new(EditorMode::full(), multi_buffer, None, window, cx);
            editor.fold_buffers(buffer_ids, cx);
            editor
        });
        editor.update_in(cx, |editor, window, cx| {
            window.focus(&editor.focus_handle(cx), cx)
        });
        cx.simulate_resize(size(px(1000.), px(1000.)));
        for expected_text in [after_first_press, after_second_press] {
            cx.update(|window, cx| window.dispatch_action(action.boxed_clone(), cx));
            cx.run_until_parked();
            editor.update(cx, |editor, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                assert_eq!(snapshot.text(), expected_text, "after {}", action.name());
                let folded = snapshot
                    .excerpts()
                    .map(|excerpt| editor.is_buffer_folded(excerpt.context.start.buffer_id, cx))
                    .collect::<Vec<_>>();
                assert_eq!(folded, [false, true], "after {}", action.name());
            });
        }
    }
}

#[gpui::test]
async fn test_transactions_unfold_the_buffer_holding_the_cursor(cx: &mut TestAppContext) {
    init_test(cx, |_| {});
    let (editor, cx) = cx.add_window_view(|window, cx| {
        let multi_buffer = MultiBuffer::build_multi(
            [
                ("a0\nb0", vec![Point::row_range(0..2)]),
                ("a1\nb1", vec![Point::row_range(0..2)]),
            ],
            cx,
        );
        let buffer_ids = multi_buffer
            .read(cx)
            .snapshot(cx)
            .excerpts()
            .map(|excerpt| excerpt.context.start.buffer_id)
            .collect::<Vec<_>>();
        let mut editor = Editor::new(EditorMode::full(), multi_buffer, None, window, cx);
        editor.fold_buffers(buffer_ids, cx);
        editor
    });
    editor.update_in(cx, |editor, window, cx| {
        editor.transact(window, cx, |editor, window, cx| {
            editor.insert("x", window, cx);
        });
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        assert_eq!(snapshot.text(), "xa0\nb0\na1\nb1");
        let folded = snapshot
            .excerpts()
            .map(|excerpt| editor.is_buffer_folded(excerpt.context.start.buffer_id, cx))
            .collect::<Vec<_>>();
        assert_eq!(folded, [false, true]);
    });
}
