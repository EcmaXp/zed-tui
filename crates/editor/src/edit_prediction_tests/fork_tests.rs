use super::*;

#[gpui::test]
async fn test_update_visible_edit_prediction_notifies_when_leading_whitespace_changes(
    cx: &mut gpui::TestAppContext,
) {
    init_test(cx, |_| {});

    let mut cx = EditorTestContext::new(cx).await;
    let provider = cx.new(|_| FakeEditPredictionDelegate::default());
    assign_editor_completion_provider(provider, &mut cx);
    cx.set_state("let x = ˇ;");
    cx.run_until_parked();
    cx.update_editor(|editor, _window, _cx| {
        assert!(!editor.in_leading_whitespace);
        editor.in_leading_whitespace = true;
    });

    let notifications = Arc::new(AtomicUsize::new(0));
    let editor = cx.editor.clone();
    let _subscription = cx.update(|_, cx| {
        let notifications = notifications.clone();
        cx.observe(&editor, move |_, _| {
            notifications.fetch_add(1, atomic::Ordering::SeqCst);
        })
    });
    cx.update_editor(|editor, window, cx| {
        editor.update_visible_edit_prediction(window, cx);
    });
    cx.run_until_parked();

    cx.update_editor(|editor, _window, _cx| {
        assert!(!editor.in_leading_whitespace);
    });
    assert_eq!(
        notifications.load(atomic::Ordering::SeqCst),
        1,
        "the key context reads in_leading_whitespace, so changing it must notify the editor",
    );
}

#[gpui::test]
async fn test_hide_context_menu_notifies_when_it_clears_a_stale_edit_prediction(
    cx: &mut gpui::TestAppContext,
) {
    init_test(cx, |_| {});

    let mut cx = EditorTestContext::new(cx).await;
    let provider = cx.new(|_| FakeEditPredictionDelegate::default());
    assign_editor_completion_provider(provider.clone(), &mut cx);
    cx.set_state("let x = ˇ;");
    propose_edits(&provider, vec![(8..8, "42")], &mut cx);
    cx.update_editor(|editor, window, cx| editor.update_visible_edit_prediction(window, cx));
    cx.update_editor(|editor, _window, cx| {
        editor.discard_edit_prediction(
            edit_prediction_types::EditPredictionDiscardReason::Ignored,
            cx,
        );
        assert!(editor.stale_edit_prediction_in_menu.is_some());
    });
    cx.run_until_parked();

    let notifications = Arc::new(AtomicUsize::new(0));
    let editor = cx.editor.clone();
    let _subscription = cx.update(|_, cx| {
        let notifications = notifications.clone();
        cx.observe(&editor, move |_, _| {
            notifications.fetch_add(1, atomic::Ordering::SeqCst);
        })
    });
    cx.update_editor(|editor, window, cx| {
        assert!(editor.hide_context_menu(window, cx).is_none());
    });
    cx.run_until_parked();

    cx.update_editor(|editor, _window, _cx| {
        assert!(editor.stale_edit_prediction_in_menu.is_none());
        assert!(editor.active_edit_prediction.is_none());
    });
    assert_eq!(
        notifications.load(atomic::Ordering::SeqCst),
        1,
        "clearing a stale edit prediction changes the cursor popover, so it must notify",
    );
}
