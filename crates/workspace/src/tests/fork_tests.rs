use super::*;

#[gpui::test]
async fn test_pane_zoom_keeps_docks_behind_the_overlay_without_a_cell_grid(
    cx: &mut TestAppContext,
) {
    let [before, zoomed, after] = layout_across_pane_zoom(cx).await;
    assert_eq!(
        (zoomed.left_panel, zoomed.right_panel),
        (before.left_panel, before.right_panel),
        "the zoom overlay covers the docks, so they should stay laid out under it"
    );
    assert_eq!(after, before, "unzooming should leave the layout as it was");
}

#[cfg(unix)]
#[test]
fn test_pane_zoom_hides_docks_on_a_cell_grid() {
    let [before, zoomed, after] = run_on_cell_grid(layout_across_pane_zoom);
    assert_eq!(
        (zoomed.left_panel, zoomed.right_panel),
        (None, None),
        "terminal themes make the zoom overlay see-through, so the docks must not render"
    );
    assert!(
        zoomed.center_item.is_some(),
        "the zoomed pane should render"
    );
    assert_eq!(
        after, before,
        "unzooming should restore the layout as it was"
    );
}

#[gpui::test]
async fn test_panel_zoom_keeps_the_layout_behind_the_overlay_without_a_cell_grid(
    cx: &mut TestAppContext,
) {
    let [before, zoomed, after] = layout_across_panel_zoom(cx).await;
    assert_eq!(
        zoomed.left_panel, before.left_panel,
        "the zoom overlay covers the other docks, so they should stay laid out under it"
    );
    assert!(
        zoomed.center_item.is_some(),
        "the zoom overlay covers the center, so it should stay laid out under it"
    );
    assert_eq!(after, before, "unzooming should leave the layout as it was");
}

#[cfg(unix)]
#[test]
fn test_panel_zoom_hides_the_center_and_other_docks_on_a_cell_grid() {
    let [before, zoomed, after] = run_on_cell_grid(layout_across_panel_zoom);
    assert_eq!(
        (zoomed.left_panel, zoomed.center_item),
        (None, None),
        "terminal themes make the zoom overlay see-through, so the layout must not render"
    );
    assert!(
        zoomed.right_panel.is_some(),
        "the zoomed panel should render"
    );
    assert_eq!(
        after, before,
        "unzooming should restore the layout as it was"
    );
}

#[gpui::test]
async fn test_focusing_a_panel_under_a_zoomed_pane_reveals_it(cx: &mut TestAppContext) {
    focus_panel_under_zoomed_pane(cx).await;
}

#[cfg(unix)]
#[test]
fn test_focusing_a_panel_hidden_by_a_zoomed_pane_reveals_it_on_a_cell_grid() {
    let [before, revealed] = run_on_cell_grid(focus_panel_under_zoomed_pane);
    assert_eq!(
        revealed, before,
        "the next frame should show the layout the zoom was hiding"
    );
}

#[cfg(unix)]
#[test]
fn test_hidden_center_hands_focus_to_a_panel_zoomed_without_focus_on_a_cell_grid() {
    run_on_cell_grid(async |cx| {
        let (workspace, right_panel, cx) = open_workspace_with_side_panels(cx).await;
        let right_dock = workspace.read_with(cx, |workspace, _| workspace.right_dock().clone());
        right_dock.update_in(cx, |dock, window, cx| {
            dock.set_panel_zoomed(&right_panel.to_any(), true, window, cx)
        });
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            assert_eq!(workspace.zoomed_position, Some(DockPosition::Right));
            assert!(
                right_panel.focus_handle(cx).contains_focused(window, cx),
                "the zoomed panel should take focus from the hidden center item"
            );
        });
        assert_eq!(cx.debug_bounds("test_item"), None);
    });
}

#[derive(Debug, PartialEq)]
struct LayoutBounds {
    left_panel: Option<Bounds<Pixels>>,
    right_panel: Option<Bounds<Pixels>>,
    center_item: Option<Bounds<Pixels>>,
}

fn layout_bounds(cx: &mut VisualTestContext) -> LayoutBounds {
    LayoutBounds {
        left_panel: cx.debug_bounds("test_panel_Left"),
        right_panel: cx.debug_bounds("test_panel_Right"),
        center_item: cx.debug_bounds("test_item"),
    }
}

#[cfg(unix)]
fn run_on_cell_grid<R>(test: impl AsyncFnOnce(&mut TestAppContext) -> R) -> R {
    let mut cx = TestAppContext::build_with_text_system(
        gpui::TestDispatcher::new(0),
        None,
        Arc::new(gpui_tui::TuiTextSystem),
    );
    let executor = cx.foreground_executor().clone();
    let result = executor.block_test(test(&mut cx));
    cx.update(|cx| cx.quit());
    cx.run_until_parked();
    result
}

async fn open_workspace_with_side_panels(
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, Entity<TestPanel>, &mut VisualTestContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, None, cx).await;
    let (workspace, cx) = cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));

    add_an_item_to_active_pane(cx, &workspace, 1);
    let right_panel = workspace.update_in(cx, |workspace, window, cx| {
        let left_panel = cx.new(|cx| TestPanel::new(DockPosition::Left, 100, cx));
        let right_panel = cx.new(|cx| TestPanel::new(DockPosition::Right, 100, cx));
        workspace.add_panel(left_panel, window, cx);
        workspace.add_panel(right_panel.clone(), window, cx);
        workspace.toggle_dock(DockPosition::Left, window, cx);
        workspace.toggle_dock(DockPosition::Right, window, cx);
        workspace.focus_center_pane(window, cx);
        right_panel
    });
    cx.run_until_parked();

    let layout = layout_bounds(cx);
    assert!(
        layout.left_panel.is_some() && layout.right_panel.is_some(),
        "both docks should be open: {layout:?}"
    );
    assert!(
        layout.center_item.is_some(),
        "the center item should render"
    );
    (workspace, right_panel, cx)
}

async fn layout_across_pane_zoom(cx: &mut TestAppContext) -> [LayoutBounds; 3] {
    let (workspace, _, cx) = open_workspace_with_side_panels(cx).await;
    let before = layout_bounds(cx);

    let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
    pane.update_in(cx, |pane, window, cx| pane.zoom_in(&ZoomIn, window, cx));
    cx.run_until_parked();
    workspace.read_with(cx, |workspace, _| {
        assert_eq!(workspace.zoomed, Some(pane.downgrade().into()));
    });
    let zoomed = layout_bounds(cx);

    pane.update_in(cx, |pane, window, cx| pane.zoom_out(&ZoomOut, window, cx));
    cx.run_until_parked();
    workspace.read_with(cx, |workspace, _| assert_eq!(workspace.zoomed, None));
    [before, zoomed, layout_bounds(cx)]
}

async fn layout_across_panel_zoom(cx: &mut TestAppContext) -> [LayoutBounds; 3] {
    let (workspace, right_panel, cx) = open_workspace_with_side_panels(cx).await;
    let before = layout_bounds(cx);

    right_panel.update(cx, |_, cx| cx.emit(PanelEvent::ZoomIn));
    cx.run_until_parked();
    workspace.read_with(cx, |workspace, _| {
        assert_eq!(workspace.zoomed_position, Some(DockPosition::Right));
    });
    let zoomed = layout_bounds(cx);

    right_panel.update(cx, |_, cx| cx.emit(PanelEvent::ZoomOut));
    cx.run_until_parked();
    workspace.read_with(cx, |workspace, cx| {
        assert_eq!(workspace.zoomed, None);
        assert!(workspace.left_dock().read(cx).is_open());
        assert!(workspace.right_dock().read(cx).is_open());
    });
    [before, zoomed, layout_bounds(cx)]
}

async fn focus_panel_under_zoomed_pane(cx: &mut TestAppContext) -> [LayoutBounds; 2] {
    let (workspace, right_panel, cx) = open_workspace_with_side_panels(cx).await;
    let before = layout_bounds(cx);
    let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
    pane.update_in(cx, |pane, window, cx| pane.zoom_in(&ZoomIn, window, cx));
    cx.run_until_parked();

    right_panel.update_in(cx, |panel, window, cx| {
        panel.focus_handle(cx).focus(window, cx)
    });
    cx.run_until_parked();

    workspace.update_in(cx, |workspace, window, cx| {
        assert_eq!(
            workspace.zoomed, None,
            "focusing a covered panel should unzoom"
        );
        assert!(!pane.read(cx).is_zoomed());
        assert!(right_panel.focus_handle(cx).is_focused(window));
    });
    [before, layout_bounds(cx)]
}
