use crate::{
    CODE_ACTIONS_DEBOUNCE_TIMEOUT, editor_tests::init_test,
    test::editor_test_context::EditorTestContext,
};
use gpui::TestAppContext;
use std::{cell::Cell, rc::Rc};

#[gpui::test]
async fn test_fetching_no_code_actions_after_none_does_not_notify(cx: &mut TestAppContext) {
    init_test(cx, |_| {});
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state("fn mainˇ() {}");
    cx.executor()
        .advance_clock(CODE_ACTIONS_DEBOUNCE_TIMEOUT * 4);
    cx.run_until_parked();

    let notifications = Rc::new(Cell::new(0));
    let editor = cx.editor.clone();
    let _subscription = cx.update(|_, cx| {
        let notifications = notifications.clone();
        cx.observe(&editor, move |_, _| {
            notifications.set(notifications.get() + 1)
        })
    });
    cx.update_editor(|editor, window, cx| {
        editor.refresh_code_actions_for_selection(window, cx);
    });
    cx.executor().advance_clock(CODE_ACTIONS_DEBOUNCE_TIMEOUT);
    cx.run_until_parked();

    assert!(!cx.editor(|editor, _, _| editor.has_available_code_actions_for_selection()));
    assert_eq!(notifications.get(), 0);
}
