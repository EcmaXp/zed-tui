use super::*;

struct SpanToNameResolver;

impl RunnableResolver for SpanToNameResolver {
    fn resolve(
        &self,
        local_captures: &[RunnableMatchCapture],
        shared_captures: &[RunnableMatchCapture],
        _buffer: &BufferSnapshot,
    ) -> Option<ResolvedRunnable> {
        let run = local_captures.iter().find(|capture| capture.is_run())?;
        let name = shared_captures
            .iter()
            .find(|capture| capture.name() == Some("_name"))?;
        Some(ResolvedRunnable {
            run_range: run.range(),
            extra_captures: SmallVec::new(),
            full_range: Some(run.range().start..name.range().end),
        })
    }
}

const SAME_NODE_GROUPED_QUERY: &str = indoc! {r#"
    (((line_comment) @run @run_item)+
      .
      (function_item
        name: (identifier) @_name))
"#};

#[gpui::test]
fn test_run_item_on_the_same_node_as_run_forms_its_own_group(cx: &mut TestAppContext) {
    let source = "// first\n// second\nfn documented() {}\n";
    let resolver: Arc<dyn RunnableResolver> = Arc::new(SpanToNameResolver);
    let runnables = collect_runnables(cx, source, SAME_NODE_GROUPED_QUERY, Some(resolver));

    let found: Vec<(String, String, Option<String>)> = runnables
        .iter()
        .map(|range| {
            (
                source[range.run_range.clone()].to_string(),
                source[range.full_range.clone()].to_string(),
                range.extra_captures.get("_name").cloned(),
            )
        })
        .collect();
    assert_eq!(
        found,
        vec![
            (
                "// first".to_string(),
                "// first\n// second\nfn documented".to_string(),
                Some("documented".to_string()),
            ),
            (
                "// second".to_string(),
                "// second\nfn documented".to_string(),
                Some("documented".to_string()),
            ),
        ],
        "each `@run_item` line should resolve with its own `@run`, the resolver's full range, and the shared captures"
    );
}

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
