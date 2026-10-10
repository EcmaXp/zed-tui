use super::super::{UPDATE_DEBOUNCE, refresh_linked_ranges};
use super::*;
use std::{cell::Cell, rc::Rc, time::Duration};

#[gpui::test]
async fn test_refresh_without_linked_ranges_does_not_notify(cx: &mut TestAppContext) {
    init_test(cx, |_| {});
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state("<divˇ></div>");
    cx.executor().advance_clock(Duration::from_secs(1));
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
        refresh_linked_ranges(editor, window, cx);
    });
    cx.executor().advance_clock(UPDATE_DEBOUNCE);
    cx.run_until_parked();

    assert_eq!(notifications.get(), 0);
}
