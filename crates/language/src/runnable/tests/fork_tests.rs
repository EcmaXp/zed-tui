use super::*;

fn run_texts_from_full_query(
    cx: &mut TestAppContext,
    source: &str,
    runnables_query: &'static str,
) -> Vec<String> {
    let language = make_language(runnables_query, None);
    let source_owned = source.to_string();
    let buffer = cx.new(|cx| Buffer::local(source_owned, cx).with_language(language, cx));
    cx.executor().run_until_parked();
    buffer.update(cx, |buffer, _| {
        let snapshot = buffer.snapshot();
        let mut matches = snapshot.matches(0..snapshot.len(), |grammar| {
            grammar.runnable_config.as_ref().map(|config| &config.query)
        });
        let mut run_texts = Vec::new();
        while let Some(mat) = matches.peek() {
            let run_capture_index = mat
                .language
                .grammar()
                .and_then(|grammar| grammar.runnable_config.as_ref())
                .and_then(|config| config.query.capture_index_for_name("run"));
            run_texts.extend(
                mat.captures
                    .iter()
                    .filter(|capture| Some(capture.index) == run_capture_index)
                    .map(|capture| text_at(&snapshot, capture.node.byte_range())),
            );
            matches.advance();
        }
        run_texts
    })
}

const COMMENTS_BEFORE_FUNCTION_QUERY: &str = indoc! {r#"
    ((line_comment) @run
     (function_item) @_function)
"#};

#[gpui::test]
fn test_runnables_raise_match_limit_without_leaking_into_pooled_cursors(cx: &mut TestAppContext) {
    let comment_count = 100;
    let source = format!(
        "{}fn commented() {{}}\n",
        "// comment\n".repeat(comment_count)
    );

    let runnables = collect_runnables(cx, &source, COMMENTS_BEFORE_FUNCTION_QUERY, None);
    assert_eq!(
        runnables.len(),
        comment_count,
        "the runnables query should keep every in-progress match up to its raised limit"
    );

    let default_limit_run_texts =
        run_texts_from_full_query(cx, &source, COMMENTS_BEFORE_FUNCTION_QUERY);
    assert!(
        default_limit_run_texts.len() < comment_count,
        "a pooled cursor reused after the runnables query should keep the default match limit, but found all {} matches",
        default_limit_run_texts.len()
    );
}
