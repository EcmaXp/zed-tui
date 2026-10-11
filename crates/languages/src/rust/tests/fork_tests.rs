use super::*;
use pretty_assertions::assert_eq;

fn rust_language_with_runnables() -> Arc<Language> {
    Arc::new(
        Language::new(
            grammars::load_config("rust"),
            Some(tree_sitter_rust::LANGUAGE.into()),
        )
        .with_queries(grammars::load_queries("rust"))
        .expect("rust queries load")
        .with_context_provider(Some(Arc::new(RustContextProvider))),
    )
}

#[derive(Debug, PartialEq)]
struct FoundRunnable {
    row: u32,
    tag: String,
    name: Option<String>,
}

fn rust_runnables(cx: &mut TestAppContext, source: &str) -> Vec<FoundRunnable> {
    let language = rust_language_with_runnables();
    let source = source.to_string();
    let buffer = cx.new(|cx| Buffer::local(source, cx).with_language(language, cx));
    cx.executor().run_until_parked();
    buffer.update(cx, |buffer, _| {
        let snapshot = buffer.snapshot();
        let mut runnables = snapshot
            .runnable_ranges(0..snapshot.len())
            .flat_map(|runnable| {
                let row = snapshot.offset_to_point(runnable.run_range.start).row;
                let name = runnable
                    .extra_captures
                    .get("_doc_test_name")
                    .or_else(|| runnable.extra_captures.get("_test_name"))
                    .cloned();
                runnable
                    .runnable
                    .tags
                    .into_iter()
                    .map(move |tag| FoundRunnable {
                        row,
                        tag: tag.0.to_string(),
                        name: name.clone(),
                    })
            })
            .collect::<Vec<_>>();
        runnables.sort_by_key(|runnable| (runnable.row, runnable.tag.clone()));
        runnables
    })
}

fn doc_test(row: u32, name: &str) -> FoundRunnable {
    FoundRunnable {
        row,
        tag: "rust-doc-test".to_string(),
        name: Some(name.to_string()),
    }
}

#[gpui::test]
fn test_rust_runnables_skip_patterns_missing_required_text(cx: &mut TestAppContext) {
    let language = rust_language_with_runnables();
    let config = language
        .grammar()
        .and_then(|grammar| grammar.runnable_config.as_ref())
        .expect("rust runnable config");
    assert!(
        std::ptr::eq(config.query_for_text(|_| true), &config.query),
        "a buffer with every required text should use the full runnables query"
    );
    assert!(
        !std::ptr::eq(config.query_for_text(|text| text != "main"), &config.query),
        "a buffer without `main` should skip the main function pattern"
    );

    let doc = "/".repeat(3);
    let source = format!(
        "{doc} Adds numbers.\nfn add() {{}}\n\n#[cfg(test)]\nmod tests {{\n    #[test]\n    fn it_works() {{}}\n}}\n"
    );
    let runnables = rust_runnables(cx, &source);
    assert_eq!(
        runnables
            .iter()
            .map(|runnable| (runnable.row, runnable.tag.as_str()))
            .collect::<Vec<_>>(),
        vec![(4, "rust-mod-test"), (6, "rust-test")],
    );

    let source_with_main = format!("{source}\nfn main() {{}}\n");
    let runnables = rust_runnables(cx, &source_with_main);
    assert_eq!(
        runnables
            .iter()
            .map(|runnable| (runnable.row, runnable.tag.as_str()))
            .collect::<Vec<_>>(),
        vec![(4, "rust-mod-test"), (6, "rust-test"), (9, "rust-main")],
    );
}

#[gpui::test]
fn test_rust_doc_test_runnables(cx: &mut TestAppContext) {
    let doc = "/".repeat(3);
    let inner_doc = format!("//{}", "!");
    let fence = "`".repeat(3);
    let source = [
        format!("{doc} Adds numbers."),
        format!("{doc} {fence}"),
        format!("{doc} assert_eq!(add(1, 2), 3);"),
        format!("{doc} {fence}"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }".to_string(),
        String::new(),
        format!("{doc} {fence}"),
        format!("{doc} let point = Point::default();"),
        format!("{doc} {fence}"),
        "#[derive(Default)]".to_string(),
        "pub struct Point;".to_string(),
        String::new(),
        "pub struct Counter;".to_string(),
        String::new(),
        "impl Counter {".to_string(),
        format!("    {doc} Creates a counter."),
        format!("    {doc}"),
        format!("    {doc} {fence}"),
        format!("    {doc} let counter = Counter::new();"),
        format!("    {doc} {fence}"),
        format!("    {doc}"),
        format!("    {doc} More details after the example."),
        "    #[must_use]".to_string(),
        "    pub fn new() -> Self { Counter }".to_string(),
        "}".to_string(),
        String::new(),
        format!("{doc} {fence}"),
        format!("{doc} m!();"),
        format!("{doc} {fence}"),
        "#[macro_export]".to_string(),
        "macro_rules! m { () => {}; }".to_string(),
        String::new(),
        format!("{doc} {fence}"),
        format!("{doc} not_a_doc_test();"),
        format!("{doc} {fence}"),
        "pub const LIMIT: usize = 1;".to_string(),
        String::new(),
        "pub fn after_const() {}".to_string(),
        String::new(),
        format!("{doc} {fence}"),
        format!("{doc} unclosed();"),
        "pub fn unclosed() {}".to_string(),
        String::new(),
    ]
    .join("\n");
    assert_eq!(
        rust_runnables(cx, &source),
        vec![
            doc_test(1, "add"),
            doc_test(6, "Point"),
            doc_test(17, "new"),
            doc_test(26, "m"),
        ],
        "doc tests attach to the documented item only, and need a closing fence"
    );

    let crate_docs = format!(
        "{inner_doc} {fence}\n{inner_doc} crate_level();\n{inner_doc} {fence}\n\npub fn crate_level() {{}}\n"
    );
    assert_eq!(
        rust_runnables(cx, &crate_docs),
        vec![doc_test(0, "crate_level")]
    );

    let language = rust_language_with_runnables();
    let buffer = cx.new(|cx| Buffer::local(source.clone(), cx).with_language(language, cx));
    cx.executor().run_until_parked();
    let add_runnable = buffer.update(cx, |buffer, _| {
        let snapshot = buffer.snapshot();
        snapshot
            .runnable_ranges(0..snapshot.len())
            .find(|runnable| {
                runnable
                    .extra_captures
                    .get("_doc_test_name")
                    .map(String::as_str)
                    == Some("add")
            })
            .map(|runnable| {
                (
                    snapshot
                        .text_for_range(runnable.full_range)
                        .collect::<String>(),
                    runnable.extra_captures,
                )
            })
    });
    let (full_text, extra_captures) = add_runnable.expect("doc test for `add`");
    assert_eq!(
        full_text,
        format!(
            "{doc} {fence}\n{doc} assert_eq!(add(1, 2), 3);\n{doc} {fence}\npub fn add(a: i32, b: i32) -> i32 {{ a + b }}"
        ),
        "the doc test spans from its opening fence to the end of the item"
    );
    assert_eq!(
        extra_captures.get("_start").map(String::as_str),
        Some(format!("{doc} {fence}\n").as_str())
    );
    assert_eq!(
        extra_captures.get("_comment_content").map(String::as_str),
        Some(format!(" {fence}\n").as_str())
    );
    assert_eq!(
        extra_captures.get("_end_code_block").map(String::as_str),
        Some(format!("{doc} {fence}\n").as_str())
    );
    assert_eq!(
        extra_captures
            .get("_end_comment_content")
            .map(String::as_str),
        Some(format!(" {fence}\n").as_str())
    );
}

#[gpui::test]
fn test_rust_test_runnables_survive_doc_comments_after_test_attribute(cx: &mut TestAppContext) {
    let doc = "/".repeat(3);
    let fence = "`".repeat(3);
    let mut lines = vec![
        format!("{doc} {fence}"),
        format!("{doc} helper();"),
        format!("{doc} {fence}"),
        "pub fn helper() {}".to_string(),
        String::new(),
        "#[cfg(test)]".to_string(),
        "mod tests {".to_string(),
    ];
    let mut expected = vec![
        doc_test(0, "helper"),
        FoundRunnable {
            row: 6,
            tag: "rust-mod-test".to_string(),
            name: None,
        },
    ];
    for index in 0..3 {
        lines.push("    #[test]".to_string());
        for line in 0..6 {
            lines.push(format!("    {doc} Line {line} of the docs."));
        }
        expected.push(FoundRunnable {
            row: lines.len() as u32,
            tag: "rust-test".to_string(),
            name: Some(format!("documented_test_{index}")),
        });
        lines.push(format!("    fn documented_test_{index}() {{}}"));
        lines.push(String::new());
    }
    lines.push("}".to_string());
    lines.push(String::new());

    assert_eq!(
        rust_runnables(cx, &lines.join("\n")),
        expected,
        "doc comments must not push `#[test]` runnables out of the query's match limit"
    );
}

#[gpui::test]
fn test_rust_runnables_survive_many_attributes_before_an_item(cx: &mut TestAppContext) {
    let doc = "/".repeat(3);
    let fence = "`".repeat(3);
    let mut lines = vec![
        format!("{doc} {fence}"),
        format!("{doc} repeat();"),
        format!("{doc} {fence}"),
    ];
    for alias in 0..12 {
        lines.push(format!("#[doc(alias = \"alias_{alias}\")]"));
    }
    lines.push("pub fn repeat() {}".to_string());
    lines.push(String::new());
    lines.push("#[cfg(test)]".to_string());
    let mod_row = lines.len() as u32;
    lines.push("mod tests {".to_string());
    lines.push("    #[rstest]".to_string());
    for case in 0..100 {
        lines.push(format!("    #[case({case})]"));
    }
    let test_row = lines.len() as u32;
    lines.push("    fn parametrized(#[case] value: u32) {}".to_string());
    lines.push("}".to_string());
    lines.push(String::new());

    assert_eq!(
        rust_runnables(cx, &lines.join("\n")),
        vec![
            doc_test(0, "repeat"),
            FoundRunnable {
                row: mod_row,
                tag: "rust-mod-test".to_string(),
                name: None,
            },
            FoundRunnable {
                row: test_row,
                tag: "rust-test".to_string(),
                name: Some("parametrized".to_string()),
            },
        ],
        "long attribute runs must not push doc test or `#[rstest]` runnables out of the query's match limit"
    );
}

#[derive(Debug, PartialEq)]
struct RunnableDetails {
    row: u32,
    tags: Vec<String>,
    run: String,
    full: String,
    extras: Vec<(String, String)>,
}

fn rust_runnable_details(cx: &mut TestAppContext, source: &str) -> Vec<RunnableDetails> {
    let language = rust_language_with_runnables();
    let source = source.to_string();
    let buffer = cx.new(|cx| Buffer::local(source, cx).with_language(language, cx));
    cx.executor().run_until_parked();
    buffer.update(cx, |buffer, _| {
        let snapshot = buffer.snapshot();
        let mut details = snapshot
            .runnable_ranges(0..snapshot.len())
            .map(|runnable| {
                let mut extras = runnable.extra_captures.into_iter().collect::<Vec<_>>();
                extras.sort();
                RunnableDetails {
                    row: snapshot.offset_to_point(runnable.run_range.start).row,
                    tags: runnable
                        .runnable
                        .tags
                        .iter()
                        .map(|tag| tag.0.to_string())
                        .collect(),
                    run: snapshot.text_for_range(runnable.run_range).collect(),
                    full: snapshot.text_for_range(runnable.full_range).collect(),
                    extras,
                }
            })
            .collect::<Vec<_>>();
        details.sort_by_key(|runnable| (runnable.row, runnable.full.clone()));
        details
    })
}

fn rust_test_details(
    row: u32,
    attribute: &str,
    start: &str,
    full: &str,
    name: &str,
) -> RunnableDetails {
    let function = full
        .lines()
        .find(|line| line.contains("fn "))
        .unwrap_or_default()
        .to_string();
    RunnableDetails {
        row,
        tags: vec!["rust-test".to_string()],
        run: name.to_string(),
        full: full.to_string(),
        extras: vec![
            ("_attribute".to_string(), attribute.to_string()),
            ("_end".to_string(), function),
            ("_start".to_string(), start.to_string()),
            ("_test_name".to_string(), name.to_string()),
        ],
    }
}

#[gpui::test]
fn test_rust_test_runnables_keep_ranges_and_captures(cx: &mut TestAppContext) {
    let doc = "/".repeat(3);
    let source = [
        "#[test]".to_string(),
        "fn plain() {}".to_string(),
        String::new(),
        "#[should_panic]".to_string(),
        "#[tokio::test]".to_string(),
        format!("{doc} Documented."),
        "async fn scoped() {}".to_string(),
        String::new(),
        "#[test_log::test]".to_string(),
        "fn logged() {}".to_string(),
        String::new(),
        "#[test]".to_string(),
        "// Interrupted.".to_string(),
        "#[ignore]".to_string(),
        "fn interrupted() {}".to_string(),
        String::new(),
        "#[cfg(test)]".to_string(),
        "fn helper() {}".to_string(),
        String::new(),
        "#[inline]".to_string(),
        "#[doc = \"test\"]".to_string(),
        "fn not_a_test() {}".to_string(),
        String::new(),
        "// A test comment.".to_string(),
        "fn commented() {}".to_string(),
        String::new(),
        "fn bare_test() {}".to_string(),
        String::new(),
    ]
    .join("\n");

    assert_eq!(
        rust_runnable_details(cx, &source),
        vec![
            rust_test_details(1, "test", "#[test]", "#[test]\nfn plain() {}", "plain"),
            rust_test_details(
                6,
                "test",
                "#[tokio::test]",
                &format!("#[tokio::test]\n{doc} Documented.\nasync fn scoped() {{}}"),
                "scoped",
            ),
            rust_test_details(
                9,
                "test_log",
                "#[test_log::test]",
                "#[test_log::test]\nfn logged() {}",
                "logged",
            ),
        ],
        "only attributes whose path names a test start a runnable, which spans from that attribute to the end of the function"
    );
}
