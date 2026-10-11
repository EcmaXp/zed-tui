use super::*;
use gpui::UpdateGlobal;
use settings::SettingsStore;

#[cfg(unix)]
#[test]
fn test_palette_on_a_cell_grid_is_its_gui_width_plus_a_frame_column() {
    let palette_cols = [(120., 40.), (105., 80.), (80., 24.), (60., 20.)].map(|(cols, rows)| {
        let mut cx = TestAppContext::build_with_text_system(
            gpui::TestDispatcher::new(0),
            None,
            Arc::new(gpui_tui::TuiTextSystem),
        );
        let executor = cx.foreground_executor().clone();
        let palette_cols = executor.block_test(palette_cols_on_cell_grid(cols, rows, &mut cx));
        cx.update(|cx| cx.quit());
        cx.run_until_parked();
        palette_cols
    });
    assert_eq!(palette_cols, [77., 77., 77., 60.]);
}

async fn palette_cols_on_cell_grid(cols: f32, rows: f32, cx: &mut TestAppContext) -> f32 {
    let app_state = init_test(cx);
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(r#"{ "ui_font_size": 10 }"#, cx)
                .result()
                .unwrap();
        });
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let (_multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let cell = cx.update(|window, _| window.text_system().cell_size().unwrap());
    cx.simulate_resize(gpui::size(cell.width * cols, cell.height * rows));
    cx.dispatch_action(Toggle);
    cx.run_until_parked();
    let palette = cx.debug_bounds("command-palette").unwrap();
    palette.size.width / cell.width
}
