use symweft_analysis::{Budget, Engine};

fn verify(path: &str, source: &str, names: &[&str], calls: &[&str]) {
    let mut engine = Engine::default();
    let update = engine.analyze(path, source.to_owned()).unwrap();
    let facts = &engine.document(path, update.revision).unwrap().facts;
    assert!(!facts.has_syntax_errors, "{path}: {facts:?}");
    assert_eq!(
        facts
            .symbols
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        names,
        "{path}"
    );
    assert_eq!(
        facts
            .calls
            .iter()
            .map(|c| c.callee.as_str())
            .collect::<Vec<_>>(),
        calls,
        "{path}"
    );
    for call in &facts.calls {
        assert_eq!(call.evidence, symweft_analysis::Evidence::Syntax);
        assert_eq!(call.resolution, symweft_analysis::Resolution::Unresolved);
        assert!(
            call.owner_id.is_some(),
            "Expected a body-contained call: {call:?}"
        );
    }
}

#[test]
fn five_grammars_extract_definitions_and_explicit_calls_without_comment_matches() {
    verify(
        "a.py",
        "# phantom()\nclass A:\n    def run(self):\n        text = 'ghost()'\n        service.work(helper())\n",
        &["A", "run"],
        &["service.work", "helper"],
    );
    verify(
        "a.c",
        "/* ghost() */\nint helper(void) { return 1; }\nint main(void) { return helper(); }\n",
        &["helper", "main"],
        &["helper"],
    );
    verify(
        "a.cpp",
        "namespace api { struct A { int run() { return helper(); } }; }\nint api::outside() { return obj.work(); }\n",
        &["api", "A", "run", "api::outside"],
        &["helper", "obj.work"],
    );
    verify(
        "a.rs",
        "struct A; impl A { fn run(&self) { service.work(helper()); println!(\"ghost()\"); } }",
        &["A", "A", "run"],
        &["service.work", "helper", "println"],
    );
    verify(
        "A.java",
        "class A { A() { setup(); } void run() { service.work(helper()); new B(); } }",
        &["A", "A", "run"],
        &["setup", "service.work", "helper", "B"],
    );
}

#[test]
fn parser_reuse_across_languages_and_incomplete_document_recovery() {
    let mut engine = Engine::default();
    for (path, source) in [
        ("a.py", "def f():\n    helper()\n"),
        ("a.c", "void f(void) { helper(); }"),
        ("a.cpp", "void f() { helper(); }"),
        ("a.rs", "fn f() { helper(); }"),
        ("A.java", "class A { void f() { helper(); } }"),
    ] {
        let opened = engine.analyze(path, source.into()).unwrap();
        let start = source.find("helper").unwrap();
        let edited = engine
            .edit(path, opened.revision, start, start + 6, "target")
            .unwrap();
        let mut cold = Engine::default();
        let fresh = cold
            .analyze(path, source.replace("helper", "target"))
            .unwrap();
        assert_eq!(
            engine.document(path, edited.revision).unwrap().facts,
            cold.document(path, fresh.revision).unwrap().facts
        );
    }
    let incomplete = engine.analyze("a.py", "def broken(".into()).unwrap();
    assert!(
        engine
            .document("a.py", incomplete.revision)
            .unwrap()
            .facts
            .has_syntax_errors
    );
    let recovered = engine
        .analyze("a.py", "def recovered():\n    target()\n".into())
        .unwrap();
    let facts = &engine.document("a.py", recovered.revision).unwrap().facts;
    assert!(!facts.has_syntax_errors);
    assert_eq!(facts.calls[0].callee, "target");
}

#[test]
fn defaults_are_not_calls_executed_by_the_function_body_and_closures_have_owners() {
    let mut engine = Engine::default();
    let update = engine
        .analyze(
            "a.py",
            "def outer(x=default()):\n    f = lambda: nested()\n    body()\n".into(),
        )
        .unwrap();
    let facts = &engine.document("a.py", update.revision).unwrap().facts;
    assert_eq!(facts.calls[0].owner_id, None);
    assert_eq!(facts.calls[1].owner_id, Some(1));
    assert_eq!(facts.calls[2].owner_id, Some(0));
}

#[test]
fn incremental_results_equal_cold_parse_after_unicode_multiline_insert_delete_and_rename() {
    let path = "a.py";
    let mut source =
        "# 😀织源\r\ndef 第一():\r\n    目标()\r\n\r\ndef second():\r\n    第一()\r\n".to_owned();
    let mut engine = Engine::default();
    let mut revision = engine.analyze(path, source.clone()).unwrap().revision;
    for (old, new) in [
        ("目标", "新目标"),
        ("# 😀织源", "# 😁织源\n# another line"),
        ("    新目标()", "    if True:\n        新目标()\n    more()"),
        ("def second():", "def changed():"),
        ("    more()\r\n", ""),
        ("    第一()", "    第一(\"hi\")"),
    ] {
        let start = source.find(old).unwrap();
        let end = start + old.len();
        let update = engine.edit(path, revision, start, end, new).unwrap();
        assert_eq!(update.mode, "incremental");
        revision = update.revision;
        source.replace_range(start..end, new);
        let mut cold = Engine::default();
        let fresh = cold.analyze(path, source.clone()).unwrap();
        assert_eq!(
            engine.document(path, revision).unwrap().facts,
            cold.document(path, fresh.revision).unwrap().facts
        );
    }
    // Whole-buffer replacement uses UTF-8 safe prefix/suffix bounds too.
    source = source.replace('😁', "😎");
    let update = engine.analyze(path, source.clone()).unwrap();
    let mut cold = Engine::default();
    let fresh = cold.analyze(path, source).unwrap();
    assert_eq!(
        engine.document(path, update.revision).unwrap().facts,
        cold.document(path, fresh.revision).unwrap().facts
    );
}

#[test]
fn unchanged_content_does_not_parse_and_stale_or_invalid_edits_preserve_snapshot() {
    let mut engine = Engine::default();
    let source = "def hello():\n    print('😀')\n";
    let first = engine.analyze("a.py", source.into()).unwrap();
    let unchanged = engine.analyze("a.py", source.into()).unwrap();
    assert_eq!(unchanged.mode, "unchanged");
    assert_eq!(unchanged.revision, first.revision);
    assert_eq!(engine.stats().parses, 1);
    assert_eq!(
        engine
            .edit("a.py", first.revision + 1, 0, 0, "")
            .unwrap_err()
            .code,
        "stale_revision"
    );
    let split = source.find('😀').unwrap() + 1;
    assert_eq!(
        engine
            .edit("a.py", first.revision, split, split, "x")
            .unwrap_err()
            .code,
        "invalid_edit"
    );
    assert_eq!(
        engine
            .context("a.py", first.revision, split)
            .unwrap_err()
            .code,
        "invalid_position"
    );
    let context = engine
        .context("a.py", first.revision, source.find("print").unwrap())
        .unwrap();
    assert_eq!(context.symbol.unwrap().name, "hello");
    assert_eq!(context.call.unwrap().callee, "print");
    assert_eq!(engine.stats().parses, 1);
}

#[test]
fn cache_evicts_inactive_documents_and_revisions_never_reuse_after_close() {
    let mut engine = Engine::new(Budget {
        max_documents: 2,
        ..Budget::default()
    });
    let a = engine.analyze("a.rs", "fn a() {}".into()).unwrap();
    let b = engine.analyze("b.rs", "fn b() {}".into()).unwrap();
    engine.analyze("a.rs", "fn a() {}".into()).unwrap();
    engine.analyze("c.rs", "fn c() {}".into()).unwrap();
    assert!(engine.document("a.rs", a.revision).is_ok());
    assert_eq!(
        engine.document("b.rs", b.revision).unwrap_err().code,
        "analysis_missing"
    );
    assert_eq!(engine.stats().cached_documents, 2);
    assert_eq!(engine.stats().evictions, 1);
    assert!(engine.close("a.rs"));
    let reopened = engine.analyze("a.rs", "fn a() {}".into()).unwrap();
    assert!(reopened.revision > a.revision);
    engine.clear();
    let switched = engine.analyze("a.rs", "fn other() {}".into()).unwrap();
    assert!(switched.revision > reopened.revision);
}

#[test]
fn failed_budget_update_is_atomic_and_unsupported_languages_do_not_allocate_documents() {
    let mut engine = Engine::new(Budget {
        max_facts: 1,
        max_document_bytes: 64,
        ..Budget::default()
    });
    let first = engine.analyze("a.rs", "fn a() {}".into()).unwrap();
    assert_eq!(
        engine
            .analyze("a.rs", "fn a() { b(); }".into())
            .unwrap_err()
            .code,
        "analysis_budget"
    );
    assert_eq!(
        engine
            .document("a.rs", first.revision)
            .unwrap()
            .facts
            .symbols[0]
            .name,
        "a"
    );
    assert_eq!(
        engine.analyze("b.py", " ".repeat(65)).unwrap_err().code,
        "analysis_budget"
    );
    assert_eq!(
        engine
            .analyze("a.swift", "func a() {}".into())
            .unwrap_err()
            .code,
        "unsupported_language"
    );
    assert_eq!(engine.stats().cached_documents, 1);
}
