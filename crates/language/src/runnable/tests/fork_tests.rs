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

const TEXT_GATED_QUERY: &str = indoc! {r#"
    ((function_item
       name: (identifier) @run
       (#eq? @run "main")) @_main)

    ((line_comment
       doc: (_) @_comment_content) @run
     (#match? @_comment_content "```")
     .
     (function_item) @_documented)

    ((function_item
       name: (identifier) @run
       (#match? @run "^test_.*")) @_test)

    ((function_item
       name: (identifier) @run
       (#not-eq? @run "helper")) @_not_helper)
"#};

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

#[test]
fn test_runnable_query_disables_patterns_missing_required_text() {
    let language = make_language(TEXT_GATED_QUERY, None);
    let config = language
        .grammar()
        .and_then(|grammar| grammar.runnable_config.as_ref())
        .expect("runnable config");

    assert!(
        std::ptr::eq(config.query_for_text(|_| true), &config.query),
        "a buffer containing every required text should use the full query"
    );

    let mut requested_texts = Vec::new();
    let without_code_fence = config.query_for_text(|text| {
        requested_texts.push(text.to_string());
        text != "```"
    });
    assert!(
        !std::ptr::eq(without_code_fence, &config.query),
        "a buffer without a code fence should use a query with the doc pattern disabled"
    );
    requested_texts.sort();
    requested_texts.dedup();
    assert_eq!(requested_texts, vec!["```", "main", "test_"]);
    assert!(
        std::ptr::eq(
            config.query_for_text(|text| text != "```"),
            without_code_fence
        ),
        "gated queries should be built once and reused"
    );
}

#[gpui::test]
fn test_text_gated_runnables_match_full_query(cx: &mut TestAppContext) {
    let doc_comment = "/".repeat(3);
    let sources = [
        format!(
            "fn main() {{}}\nfn helper() {{}}\nfn test_alpha() {{}}\n{doc_comment} Example\nfn documented() {{}}\n"
        ),
        format!(
            "fn helper() {{}}\nfn test_alpha() {{}}\n{doc_comment} ```\nfn documented() {{}}\n"
        ),
        "fn helper() {}\nfn other() {}\n".to_string(),
    ];
    for source in &sources {
        let gated: Vec<String> = collect_runnables(cx, source, TEXT_GATED_QUERY, None)
            .iter()
            .map(|range| source[range.run_range.clone()].to_string())
            .collect();
        let full = run_texts_from_full_query(cx, source, TEXT_GATED_QUERY);
        assert_eq!(
            gated, full,
            "text gating must not change the runnables found in {source:?}"
        );
    }
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

#[test]
fn test_rope_contains_finds_text_across_chunk_boundaries() {
    let needle = "```";
    for position in 0..400 {
        let mut text = "a".repeat(400);
        text.insert_str(position, needle);
        assert!(
            rope_contains(&Rope::from(text.as_str()), needle),
            "needle at byte {position} should be found"
        );
    }
    assert!(!rope_contains(
        &Rope::from("a``b``c".repeat(100).as_str()),
        needle
    ));
    assert!(rope_contains(&Rope::from(""), ""));
    assert!(!rope_contains(&Rope::from(""), needle));
}
