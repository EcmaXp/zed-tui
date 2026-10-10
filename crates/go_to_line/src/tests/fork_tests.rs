use super::*;

#[gpui::test]
async fn test_cursor_position_updates_before_tasks_run_in_singleton_buffers(
    cx: &mut TestAppContext,
) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/dir"),
        json!({
            "a.rs": "first\nsecond"
        }),
    )
    .await;

    let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    workspace.update_in(cx, |workspace, window, cx| {
        let cursor_position = cx.new(|_| CursorPosition::new(workspace));
        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_right_item(cursor_position, window, cx);
        });
    });

    let worktree_id = workspace.update(cx, |workspace, cx| {
        workspace.project().update(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        })
    });
    let _buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/dir/a.rs"), cx)
        })
        .await
        .unwrap();
    let editor = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path((worktree_id, rel_path("a.rs")), None, true, window, cx)
        })
        .await
        .unwrap()
        .downcast::<Editor>()
        .unwrap();

    editor.update_in(cx, |editor, window, cx| {
        editor.move_to_beginning(&MoveToBeginning, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(user_caret_position(1, 1), current_position(&workspace, cx));

    editor.update_in(cx, |editor, window, cx| {
        editor.move_right(&MoveRight, window, cx)
    });
    assert_eq!(
        user_caret_position(1, 2),
        current_position(&workspace, cx),
        "The status bar should read the new position while drawing the frame that moves the caret",
    );

    editor.update_in(cx, |editor, window, cx| {
        editor.select_all(&SelectAll, window, cx)
    });
    workspace.update(cx, |workspace, cx| {
        assert_eq!(
            &SelectionStats {
                lines: 2,
                characters: 12,
                selections: 1,
            },
            workspace
                .status_bar()
                .read(cx)
                .item_of_type::<CursorPosition>()
                .expect("missing cursor position item")
                .read(cx)
                .selection_stats(),
            "Selection stats should also update without waiting for a task",
        );
    });
}

#[gpui::test]
async fn test_cursor_position_reads_selections_once_per_frame(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/dir"),
        json!({
            "a.rs": "first\nsecond"
        }),
    )
    .await;

    let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let cursor_position = workspace.update_in(cx, |workspace, window, cx| {
        let cursor_position = cx.new(|_| CursorPosition::new(workspace));
        workspace.status_bar().update(cx, |status_bar, cx| {
            status_bar.add_right_item(cursor_position.clone(), window, cx);
        });
        cursor_position
    });

    let worktree_id = workspace.update(cx, |workspace, cx| {
        workspace.project().update(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        })
    });
    let _buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/dir/a.rs"), cx)
        })
        .await
        .unwrap();
    let editor = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path((worktree_id, rel_path("a.rs")), None, true, window, cx)
        })
        .await
        .unwrap()
        .downcast::<Editor>()
        .unwrap();
    editor.update_in(cx, |editor, window, cx| {
        editor.move_to_beginning(&MoveToBeginning, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(user_caret_position(1, 1), current_position(&workspace, cx));

    let reads_before =
        cursor_position.read_with(cx, |cursor_position, _| cursor_position.position_reads());
    editor.update_in(cx, |editor, window, cx| {
        editor.move_right(&MoveRight, window, cx);
        editor.move_right(&MoveRight, window, cx);
        editor.move_right(&MoveRight, window, cx);
    });
    assert_eq!(user_caret_position(1, 4), current_position(&workspace, cx));
    assert_eq!(
        cursor_position.read_with(cx, |cursor_position, _| cursor_position.position_reads()),
        reads_before + 1,
        "three selection changes in one update should read the selections once, for the frame they share",
    );

    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert_eq!(
        cursor_position.read_with(cx, |cursor_position, _| cursor_position.position_reads()),
        reads_before + 1,
        "a frame without selection changes should not read the selections again",
    );
}
