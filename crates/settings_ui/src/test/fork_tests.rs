use super::*;

fn settings_window_with_all_pages(
    cx: &mut gpui::TestAppContext,
) -> (Entity<SettingsWindow>, &mut gpui::VisualTestContext) {
    cx.update(|cx| {
        register_settings(cx);
        let app_state = AppState::test(cx);
        AppState::set_global(app_state, cx);
    });
    cx.add_window_view(|window, cx| {
        let mut settings_window = SettingsWindow::test(window, cx);
        settings_window.pages = page_data::settings_data(cx);
        settings_window.build_filter_table();
        settings_window.build_navbar(cx);
        settings_window.build_search_index();
        settings_window.filter_matches_to_file(cx);
        settings_window
    })
}

fn pin_terminal_settings(cx: &mut App) {
    cx.update_global::<SettingsStore, _>(|store, cx| {
        store
            .set_server_settings(
                &util::asset_str::<settings::SettingsAssets>("settings/tui_constraints.json"),
                cx,
            )
            .unwrap();
    });
}

fn titles_hidden_by_server_settings(cx: &mut gpui::TestAppContext) -> Vec<&'static str> {
    let (settings_window, cx) = settings_window_with_all_pages(cx);
    settings_window.update_in(cx, |settings_window, _, cx| {
        let without_server_settings = settings_window.filter_table.clone();
        pin_terminal_settings(cx);
        settings_window.build_filter_table();
        settings_window.filter_matches_to_file(cx);

        let pages = settings_window.pages.iter().zip(
            without_server_settings
                .iter()
                .zip(&settings_window.filter_table),
        );
        pages
            .flat_map(|(page, (before, after))| page.items.iter().zip(before.iter().zip(after)))
            .filter(|(_, (shown_before, shown_after))| **shown_before && !**shown_after)
            .filter_map(|(item, _)| match item {
                SettingsPageItem::SettingItem(item)
                | SettingsPageItem::DynamicItem(DynamicItem {
                    discriminant: item, ..
                }) => Some(item.title),
                _ => None,
            })
            .collect()
    })
}

#[gpui::test]
fn server_pinned_settings_are_hidden(cx: &mut gpui::TestAppContext) {
    assert_eq!(
        titles_hidden_by_server_settings(cx),
        [
            "On Last Window Closed",
            "Use System Path Prompts",
            "Use System Prompts",
            "Auto Update",
            "Font Size",
            "Line Height",
            "Font Size",
            "UI Font Size",
            "Buffer Font Size",
            "Font Size",
            "Cursor Blink",
            "Rounded Selection",
            "Mouse Wheel Zoom",
            "Show Bookmarks",
            "Show Folds",
            "Min Line Number Digits",
            "Border Size",
            "Font Size",
            "Cursor Blinking",
        ]
    );
}

#[gpui::test]
fn searching_for_a_server_pinned_setting_shows_nothing(cx: &mut gpui::TestAppContext) {
    let (settings_window, cx) = settings_window_with_all_pages(cx);
    settings_window.update_in(cx, |settings_window, _, cx| {
        pin_terminal_settings(cx);
        let query = "#mouse_wheel_zoom";
        let indices = settings_window.filter_by_json_path(query);
        assert_eq!(indices.len(), 1);
        settings_window.apply_match_indices(indices.into_iter(), query, cx);
        let shown_items = settings_window
            .filter_table
            .iter()
            .flatten()
            .filter(|shown| **shown)
            .count();
        assert_eq!(shown_items, 0);
    });
}
