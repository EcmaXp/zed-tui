use super::*;

#[gpui::test]
async fn test_refreshing_unchanged_outline_symbols_does_not_notify(cx: &mut TestAppContext) {
    init_test(cx, |_| {});

    let mut cx = EditorLspTestContext::new_rust(lsp::ServerCapabilities::default(), cx).await;
    cx.set_state("fn maˇin() {\n    let x = 1;\n}\n\nfn other() {}\n");
    cx.run_until_parked();
    cx.update_editor(|editor, _window, _cx| {
        assert_eq!(outline_symbol_names(editor), vec!["fn main"]);
    });

    let notifications = Arc::new(atomic::AtomicUsize::new(0));
    let symbol_events = Arc::new(atomic::AtomicUsize::new(0));
    let editor = cx.editor.clone();
    let _subscriptions = cx.update(|_, cx| {
        let notifications = notifications.clone();
        let symbol_events = symbol_events.clone();
        [
            cx.observe(&editor, move |_, _| {
                notifications.fetch_add(1, atomic::Ordering::SeqCst);
            }),
            cx.subscribe(&editor, move |_, event, _| {
                if matches!(event, crate::EditorEvent::OutlineSymbolsChanged) {
                    symbol_events.fetch_add(1, atomic::Ordering::SeqCst);
                }
            }),
        ]
    });

    cx.update_editor(|editor, _window, cx| editor.refresh_outline_symbols_at_cursor(cx));
    cx.run_until_parked();
    assert_eq!(
        notifications.load(atomic::Ordering::SeqCst),
        0,
        "unchanged outline symbols at the cursor should not notify the editor",
    );
    assert_eq!(symbol_events.load(atomic::Ordering::SeqCst), 0);

    cx.update_editor(|editor, _window, cx| editor.update_outline_symbols_at_cursor(true, cx));
    cx.run_until_parked();
    assert_eq!(
        symbol_events.load(atomic::Ordering::SeqCst),
        1,
        "new document symbols must still reach the outline panel even when the cursor symbols are unchanged",
    );

    cx.set_selections_state("fn main() {\n    let x = 1;\n}\n\nfn otˇher() {}\n");
    cx.run_until_parked();
    cx.update_editor(|editor, _window, _cx| {
        assert_eq!(outline_symbol_names(editor), vec!["fn other"]);
    });
    assert_eq!(
        symbol_events.load(atomic::Ordering::SeqCst),
        2,
        "moving into another symbol should report the change",
    );
}
