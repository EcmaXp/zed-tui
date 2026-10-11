use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use gpui::AppContext as _;

use super::ModeIndicator;
use crate::test::VimTestContext;

#[gpui::test]
async fn test_mode_indicator_notifies_only_when_pending_keys_change(cx: &mut gpui::TestAppContext) {
    let mut cx = VimTestContext::new(cx, false).await;
    cx.run_until_parked();

    let mode_indicator = cx.workspace(|workspace, _, cx| {
        workspace
            .status_bar()
            .read(cx)
            .item_of_type::<ModeIndicator>()
            .expect("missing mode indicator")
    });
    let notifications = Arc::new(AtomicUsize::new(0));
    let _subscription = cx.update(|_, cx| {
        let notifications = notifications.clone();
        cx.observe(&mode_indicator, move |_, _| {
            notifications.fetch_add(1, Ordering::SeqCst);
        })
    });

    cx.simulate_keystrokes("h j");
    cx.run_until_parked();
    cx.assert_editor_state("hjˇ");
    assert_eq!(
        notifications.load(Ordering::SeqCst),
        0,
        "keys that leave no pending input should not notify the mode indicator",
    );

    cx.simulate_keystrokes("cmd-k");
    cx.run_until_parked();
    assert!(cx.read_entity(&mode_indicator, |mode_indicator, _| {
        mode_indicator.pending_keys.is_some()
    }));
    assert_eq!(
        notifications.load(Ordering::SeqCst),
        1,
        "starting a multi-key binding should notify the mode indicator",
    );
}
