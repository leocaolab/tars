use super::*;

fn canon(model: &str, body: serde_json::Value) -> String {
    format!("model={model}\0{body}")
}

/// The whole point: say WHERE it changed, and name the part.
#[test]
fn a_changed_field_is_located_by_pointer_and_named_by_component() {
    let a = canon(
        "m",
        serde_json::json!({"system": "be careful", "messages": []}),
    );
    let b = canon(
        "m",
        serde_json::json!({"system": "be careful now", "messages": []}),
    );
    let d = RequestDiff::build(&b, &a);

    assert_eq!(
        d.changes.len(),
        1,
        "only the differing field appears: {:?}",
        d.changes
    );
    let c = &d.changes[0];
    assert_eq!(c.path, "/system");
    assert_eq!(c.component.as_deref(), Some("system-prompt"));
    assert_eq!(c.op, ChangeKind::Changed);
    assert_eq!(c.old.as_deref(), Some("be careful"));
    assert_eq!(c.new.as_deref(), Some("be careful now"));
}

/// Identical fields must not appear — a diff that reprints the whole
/// request buries its own conclusion.
#[test]
fn identical_fields_are_omitted_entirely() {
    let same = serde_json::json!({"system": "s", "tools": [{"name": "read"}]});
    let d = RequestDiff::build(&canon("m", same.clone()), &canon("m", same));
    assert!(
        d.changes.is_empty(),
        "no changes for identical requests: {:?}",
        d.changes
    );
    assert_eq!(d.summary.changed, 0);
}

/// A changed value is NEVER shortened in the record, however long.
#[test]
fn the_record_keeps_a_changed_value_in_full() {
    let long = "x".repeat(50_000);
    let a = canon("m", serde_json::json!({"system": "short"}));
    let b = canon("m", serde_json::json!({"system": long}));
    let d = RequestDiff::build(&b, &a);
    assert_eq!(
        d.changes[0].new.as_deref().map(str::len),
        Some(50_000),
        "storage must not truncate — that would discard the record's only content"
    );
}

/// Folding hides, announces, and points at the original — the property that
/// separates it from truncation.
#[test]
fn folding_announces_its_size_and_where_to_read_the_original() {
    let many = (0..40)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let a = canon("m", serde_json::json!({"system": "one line"}));
    let b = canon("m", serde_json::json!({"system": many}));
    let d = RequestDiff::build(&b, &a);

    let folded = d.render(FoldDepth::Folded, Some("/tmp/x.json"));
    assert!(
        folded.contains("folded 34 line(s)"),
        "says how many lines are hidden:\n{folded}"
    );
    assert!(
        folded.contains("/tmp/x.json:/system"),
        "points at the original:\n{folded}"
    );
    assert!(
        folded.contains("line 0") && folded.contains("line 39"),
        "keeps both edges:\n{folded}"
    );
    assert!(
        !folded.contains("line 20"),
        "the middle is hidden:\n{folded}"
    );

    let full = d.render(FoldDepth::Full, Some("/tmp/x.json"));
    assert!(full.contains("line 20"), "Full hides nothing:\n{full}");
}

/// tars names only the shapes it owns; anything else stays unnamed rather
/// than being given an invented component.
#[test]
fn an_unknown_path_is_left_unnamed_not_guessed() {
    let a = canon("m", serde_json::json!({"vendor_extension": {"k": 1}}));
    let b = canon("m", serde_json::json!({"vendor_extension": {"k": 2}}));
    let d = RequestDiff::build(&b, &a);
    assert_eq!(d.changes[0].component, None);
    assert_eq!(d.changes[0].path, "/vendor_extension/k");
}

#[test]
fn added_and_removed_are_distinguished_from_changed() {
    let a = canon("m", serde_json::json!({"tools": [{"name": "read"}]}));
    let b = canon(
        "m",
        serde_json::json!({"tools": [{"name": "read"}, {"name": "write"}]}),
    );
    let d = RequestDiff::build(&b, &a);
    assert_eq!(d.summary.added, 1, "{:?}", d.changes);
    assert_eq!(d.changes[0].op, ChangeKind::Added);
    assert_eq!(d.changes[0].component.as_deref(), Some("tool-specs"));
}

/// The model participates in the fingerprint, so a model swap must show up
/// as a change rather than as an unexplained miss.
#[test]
fn a_model_change_is_reported() {
    let body = serde_json::json!({"system": "s"});
    let d = RequestDiff::build(&canon("m2", body.clone()), &canon("m1", body));
    assert_eq!(d.changes.len(), 1);
    assert_eq!(d.changes[0].path, "/model");
    assert_eq!(d.changes[0].old.as_deref(), Some("m1"));
}

mod compare {
    use super::super::*;
    use std::collections::BTreeMap;
    use tars_llm::provider::backends::cassette::Recording;
    use tars_types::OutputLimit;

    const MODEL: &str = "m";

    fn recorded_request() -> ChatRequest {
        let mut r = ChatRequest::user("review lib.rs").with_system("be careful");
        r.max_output_tokens = Some(100);
        r
    }

    /// Returns a cassette holding `req` and the key it is recorded under.
    fn cassette_of(
        req: &ChatRequest,
        output_limits: BTreeMap<String, OutputLimit>,
    ) -> (CassetteFile, String) {
        let key = request_fingerprint(req, MODEL);
        let recording = Recording {
            request: canon_request(req, MODEL),
            events: vec![],
        };
        let file = CassetteFile::new(None, output_limits, [recording]);
        (file, key)
    }

    #[test]
    fn a_recorded_request_replays() {
        let (file, key) = cassette_of(&recorded_request(), BTreeMap::new());
        assert!(compare(&file, &recorded_request(), MODEL, &key).is_ok());
    }

    #[test]
    fn an_unset_output_limit_is_filled_from_the_recorded_limit_as_the_service_fills_it() {
        let limits = BTreeMap::from([(MODEL.to_string(), OutputLimit::Configured { tokens: 100 })]);
        let (file, key) = cassette_of(&recorded_request(), limits);
        let mut unset = recorded_request();
        unset.max_output_tokens = None;
        assert!(compare(&file, &unset, MODEL, &key).is_ok());
    }

    #[test]
    fn an_unset_output_limit_without_a_recorded_limit_is_a_parameter_difference() {
        let (file, key) = cassette_of(&recorded_request(), BTreeMap::new());
        let mut unset = recorded_request();
        unset.max_output_tokens = None;
        let err = compare(&file, &unset, MODEL, &key).unwrap_err();
        assert!(matches!(err, CassetteDiffError::Params(_)), "{err:?}");
    }

    #[test]
    fn an_unrecorded_request_without_its_expected_recording_is_a_miss() {
        let (file, _) = cassette_of(&recorded_request(), BTreeMap::new());
        let other = ChatRequest::user("uncovered");
        match compare(&file, &other, MODEL, "no-such-key") {
            Err(CassetteDiffError::Miss {
                fingerprint,
                expected,
            }) => {
                assert_eq!(fingerprint, request_fingerprint(&other, MODEL));
                assert_eq!(expected, "no-such-key");
            }
            other => panic!("expected a miss, got {other:?}"),
        }
    }

    #[test]
    fn a_changed_system_prompt_is_a_prompt_difference() {
        let (file, key) = cassette_of(&recorded_request(), BTreeMap::new());
        let mut changed = recorded_request();
        changed.system = Some("be careful now".into());
        match compare(&file, &changed, MODEL, &key) {
            Err(CassetteDiffError::Prompt(d)) => assert_eq!(d.changes[0].path, "/system"),
            other => panic!("expected a prompt difference, got {other:?}"),
        }
    }

    #[test]
    fn a_changed_message_is_a_context_difference() {
        let (file, key) = cassette_of(&recorded_request(), BTreeMap::new());
        let changed = {
            let mut r = ChatRequest::user("review main.rs").with_system("be careful");
            r.max_output_tokens = Some(100);
            r
        };
        let err = compare(&file, &changed, MODEL, &key).unwrap_err();
        assert!(matches!(err, CassetteDiffError::Context(_)), "{err:?}");
    }

    #[test]
    fn a_changed_sampling_parameter_is_a_parameter_difference() {
        let (file, key) = cassette_of(&recorded_request(), BTreeMap::new());
        let mut changed = recorded_request();
        changed.temperature = Some(0.5);
        let err = compare(&file, &changed, MODEL, &key).unwrap_err();
        assert!(matches!(err, CassetteDiffError::Params(_)), "{err:?}");
    }

    #[test]
    fn a_changed_prompt_and_message_is_named_by_the_prompt_and_carries_both() {
        let (file, key) = cassette_of(&recorded_request(), BTreeMap::new());
        let changed = {
            let mut r = ChatRequest::user("review main.rs").with_system("be careless");
            r.max_output_tokens = Some(100);
            r
        };
        match compare(&file, &changed, MODEL, &key) {
            Err(CassetteDiffError::Prompt(d)) => assert!(d.changes.len() >= 2, "{:?}", d.changes),
            other => panic!("expected a prompt difference, got {other:?}"),
        }
    }

    #[test]
    fn a_model_named_with_the_prefix_is_a_model_change() {
        let d = RequestDiff::build("model=model=x\0{}", "model=x\0{}");
        assert_eq!(d.changes.len(), 1, "{:?}", d.changes);
        assert_eq!(d.changes[0].new.as_deref(), Some("model=x"));
    }

    #[test]
    fn texts_that_differ_only_outside_both_halves_are_one_whole_text_change() {
        let d = RequestDiff::build("{}", "model=\0{}");
        assert_eq!(d.changes.len(), 1, "{:?}", d.changes);
        assert_eq!(d.changes[0].path, "");
        assert!(
            RequestDiff::build("model=m\0{}", "model=m\0{}")
                .changes
                .is_empty()
        );
    }

    #[test]
    fn an_error_shows_the_summary_and_names_no_record_it_was_not_given() {
        let (file, key) = cassette_of(&recorded_request(), BTreeMap::new());
        let mut changed = recorded_request();
        changed.system = Some("be careful now".into());
        let said = compare(&file, &changed, MODEL, &key)
            .unwrap_err()
            .to_string();
        assert!(said.contains("1 changed"), "{said}");
        assert!(
            !said.contains("full record") && !said.contains("full at"),
            "{said}"
        );
    }

    #[test]
    fn bodies_that_parse_equal_but_differ_in_bytes_are_one_whole_body_change() {
        let d = RequestDiff::build("model=m\0{\"a\":1,\"b\":2}", "model=m\0{\"b\":2,\"a\":1}");
        assert_eq!(d.changes.len(), 1, "{:?}", d.changes);
        assert_eq!(d.changes[0].path, "/");
    }
}
