use super::*;

#[gpui::test]
async fn test_runnables_wait_for_typing_to_pause(cx: &mut TestAppContext) {
    init_test(cx, |_| {});

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/project"), json!({ "main.rs": "fn main() {}\n" }))
        .await;
    let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang_with_task_context());
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/project/main.rs"), cx)
        })
        .await
        .unwrap();
    let buffer_id = buffer.read_with(cx, |buffer, _| buffer.remote_id());
    let multi_buffer = cx.new(|cx| MultiBuffer::singleton(buffer.clone(), cx));
    let editor = cx.add_window(|window, cx| {
        Editor::for_multibuffer(multi_buffer, Some(project.clone()), window, cx)
    });
    cx.executor().advance_clock(Duration::from_millis(500));
    cx.executor().run_until_parked();
    let labels = |cx: &mut TestAppContext| {
        editor
            .update(cx, |editor, _, _| collect_runnable_labels(editor))
            .unwrap()
    };
    assert_eq!(
        labels(cx),
        vec![(buffer_id, 0, vec!["Run main".to_string()])]
    );

    for _ in 0..4 {
        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "\n")], None, cx));
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.executor().run_until_parked();
        assert_eq!(
            labels(cx),
            vec![(buffer_id, 0, vec!["Run main".to_string()])],
            "runnables should not be rescanned while edits keep arriving"
        );
    }

    cx.executor()
        .advance_clock(super::super::WHOLE_BUFFER_RUNNABLES_DEBOUNCE);
    cx.executor().run_until_parked();
    assert_eq!(
        labels(cx),
        vec![(buffer_id, 4, vec!["Run main".to_string()])]
    );
}
