use super::*;
use pretty_assertions::assert_eq;

#[gpui::test]
async fn test_indent_guides_stop_at_excerpt_boundaries(cx: &mut TestAppContext) {
    init_test(cx, |_| {});

    let buffer_a = cx.new(|cx| {
        Buffer::local(
            indoc! {"
                fn first() {
                    if a {
                        one();
                        two();
                    }
                    if b {
                        three();
                        four();
                    }
                }
            "},
            cx,
        )
    });
    let buffer_b = cx.new(|cx| {
        Buffer::local(
            indoc! {"
                fn second() {
                    if c {
                        five();
                    }
                }
            "},
            cx,
        )
    });
    let multi_buffer = cx.new(|cx| {
        let mut multi_buffer = MultiBuffer::new(ReadWrite);
        multi_buffer.set_excerpts_for_path(
            PathKey::sorted(0),
            buffer_a.clone(),
            [
                Point::new(2, 0)..Point::new(3, 0),
                Point::new(6, 0)..Point::new(7, 0),
            ],
            0,
            cx,
        );
        multi_buffer.set_excerpts_for_path(
            PathKey::sorted(1),
            buffer_b.clone(),
            [Point::new(2, 0)..Point::new(2, 0)],
            0,
            cx,
        );
        multi_buffer
    });
    let (editor, cx) = cx.add_window_view(|window, cx| {
        Editor::new(EditorMode::full(), multi_buffer, None, window, cx)
    });

    let (text, guides) = editor.update_in(cx, |editor, window, cx| {
        let snapshot = editor.snapshot(window, cx).display_snapshot;
        let max_row = snapshot.buffer_snapshot().max_point().row;
        let mut guides = crate::indent_guides::indent_guides_in_range(
            editor,
            MultiBufferRow(0)..MultiBufferRow(max_row),
            true,
            &snapshot,
            cx,
        )
        .into_iter()
        .map(|guide| {
            (
                guide.buffer_id,
                guide.start_row.0..=guide.end_row.0,
                guide.depth,
            )
        })
        .collect::<Vec<_>>();
        guides.sort_by_key(|(_, rows, depth)| (*rows.start(), *depth));
        (snapshot.buffer_snapshot().text(), guides)
    });

    assert_eq!(
        text,
        "        one();\n        two();\n        three();\n        four();\n        five();"
    );
    let buffer_a = buffer_a.read_with(cx, |buffer, _| buffer.remote_id());
    let buffer_b = buffer_b.read_with(cx, |buffer, _| buffer.remote_id());
    assert_eq!(
        guides,
        vec![
            (buffer_a, 0..=1, 0),
            (buffer_a, 0..=1, 1),
            (buffer_a, 2..=3, 0),
            (buffer_a, 2..=3, 1),
            (buffer_b, 4..=4, 0),
            (buffer_b, 4..=4, 1),
        ]
    );
}
