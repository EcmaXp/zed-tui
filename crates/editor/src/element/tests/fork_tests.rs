use super::*;

#[cfg(unix)]
#[test]
fn test_tui_expand_excerpt_toggles_hover_only_their_own_row() {
    let mut cx = TestAppContext::build_with_text_system(
        gpui::TestDispatcher::new(0),
        None,
        Arc::new(gpui_tui::TuiTextSystem),
    );
    init_test(&mut cx, |_| {});
    cx.update(|cx| {
        settings::SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(
                    r#"{
                        "ui_font_size": 10,
                        "buffer_font_size": 16,
                        "buffer_line_height": { "custom": 1.0 },
                        "gutter": { "folds": false, "min_line_number_digits": 3 }
                    }"#,
                    cx,
                )
                .result()
                .unwrap();
        });
    });
    let text = (0..300)
        .map(|row| format!("line {row}\n"))
        .collect::<String>();
    let buffer = cx.new(|cx| Buffer::local(text, cx));
    let multi_buffer = cx.new(|_| MultiBuffer::new(Capability::ReadWrite));
    multi_buffer.update(&mut cx, |multi_buffer, cx| {
        multi_buffer.set_excerpts_for_path(
            PathKey::for_buffer(&buffer, cx),
            buffer.clone(),
            [
                Point::new(100, 0)..Point::new(100, 0),
                Point::new(200, 0)..Point::new(200, 0),
            ],
            1,
            cx,
        );
    });
    let window =
        cx.add_window(|window, cx| Editor::new(EditorMode::full(), multi_buffer, None, window, cx));
    cx.simulate_window_resize(window.into(), size(px(640.), px(320.)));
    let cx = &mut VisualTestContext::from_window(*window, &mut cx);
    cx.run_until_parked();

    let boundary_row = window
        .update(cx, |editor, window, cx| {
            let snapshot = editor.snapshot(window, cx);
            snapshot
                .blocks_in_range(DisplayRow(0)..snapshot.max_point().row())
                .find_map(|(row, block)| {
                    matches!(block, Block::ExcerptBoundary { .. }).then_some(row.0 as i32)
                })
        })
        .unwrap()
        .expect("an excerpt boundary between the two excerpts");

    let painted_quads = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            let scale = window.scale_factor();
            window
                .painted_quads()
                .into_iter()
                .map(|quad| {
                    let top = (quad.bounds.top().0 / scale / 16.) as i32;
                    let bottom = (quad.bounds.bottom().0 / scale / 16.) as i32;
                    (top..bottom, format!("{:?}", quad.background))
                })
                .collect::<Vec<_>>()
        })
    };
    cx.simulate_mouse_move(point(px(636.), px(316.)), None, gpui::Modifiers::none());
    cx.run_until_parked();
    let unhovered_quads = painted_quads(cx);
    let mut hovered_rows = |row: i32| {
        let mut rows = Vec::new();
        for col in 0..4 {
            cx.simulate_mouse_move(
                point(px(col as f32 * 8. + 4.), px(row as f32 * 16. + 8.)),
                None,
                gpui::Modifiers::none(),
            );
            cx.run_until_parked();
            for (quad_rows, _) in painted_quads(cx)
                .into_iter()
                .filter(|quad| !unhovered_quads.contains(quad))
            {
                if !rows.contains(&quad_rows) {
                    rows.push(quad_rows);
                }
            }
        }
        rows
    };

    assert_eq!(
        hovered_rows(boundary_row - 1),
        [boundary_row - 1..boundary_row]
    );
    assert_eq!(hovered_rows(boundary_row), Vec::<Range<i32>>::new());
    assert_eq!(
        hovered_rows(boundary_row + 1),
        [boundary_row + 1..boundary_row + 2]
    );
    assert_eq!(hovered_rows(boundary_row + 2), Vec::<Range<i32>>::new());
}
