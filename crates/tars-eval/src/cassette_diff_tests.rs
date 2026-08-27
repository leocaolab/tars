// Tests for the cassette request diff. `include!`d so they share the module's
// private items without widening the API for testing.

#[cfg(test)]
mod tests {
    use super::*;

    fn fps() -> Fingerprints {
        Fingerprints { want: "want".into(), baseline: "base".into() }
    }

    fn canon(model: &str, body: serde_json::Value) -> String {
        format!("model={model}\0{body}")
    }

    /// The whole point: say WHERE it changed, and name the part.
    #[test]
    fn a_changed_field_is_located_by_pointer_and_named_by_component() {
        let a = canon("m", serde_json::json!({"system": "be careful", "messages": []}));
        let b = canon("m", serde_json::json!({"system": "be careful now", "messages": []}));
        let d = RequestDiff::build(&b, &a, fps(), BaselineBy::Label);

        assert_eq!(d.changes.len(), 1, "only the differing field appears: {:?}", d.changes);
        let c = &d.changes[0];
        assert_eq!(c.path, "/system");
        assert_eq!(c.component.as_deref(), Some("system-prompt"));
        assert_eq!(c.op, Op::Changed);
        assert_eq!(c.old.as_deref(), Some("be careful"));
        assert_eq!(c.new.as_deref(), Some("be careful now"));
    }

    /// Identical fields must not appear — a diff that reprints the whole
    /// request buries its own conclusion.
    #[test]
    fn identical_fields_are_omitted_entirely() {
        let same = serde_json::json!({"system": "s", "tools": [{"name": "read"}]});
        let d = RequestDiff::build(&canon("m", same.clone()), &canon("m", same), fps(), BaselineBy::Seq);
        assert!(d.changes.is_empty(), "no changes for identical requests: {:?}", d.changes);
        assert_eq!(d.summary.changed, 0);
    }

    /// A changed value is NEVER shortened in the record, however long.
    #[test]
    fn the_record_keeps_a_changed_value_in_full() {
        let long = "x".repeat(50_000);
        let a = canon("m", serde_json::json!({"system": "short"}));
        let b = canon("m", serde_json::json!({"system": long}));
        let d = RequestDiff::build(&b, &a, fps(), BaselineBy::Label);
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
        let many = (0..40).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let a = canon("m", serde_json::json!({"system": "one line"}));
        let b = canon("m", serde_json::json!({"system": many}));
        let d = RequestDiff::build(&b, &a, fps(), BaselineBy::Label);

        let folded = d.render(Fold::Folded, Some("/tmp/x.json"));
        assert!(folded.contains("folded 34 line(s)"), "says how many lines are hidden:\n{folded}");
        assert!(folded.contains("/tmp/x.json:/system"), "points at the original:\n{folded}");
        assert!(folded.contains("line 0") && folded.contains("line 39"), "keeps both edges:\n{folded}");
        assert!(!folded.contains("line 20"), "the middle is hidden:\n{folded}");

        let full = d.render(Fold::Full, Some("/tmp/x.json"));
        assert!(full.contains("line 20"), "Full hides nothing:\n{full}");
    }

    /// A prefix-picked baseline must announce that it is a guess: a diff against
    /// the wrong baseline points at a change that never happened.
    #[test]
    fn a_guessed_baseline_says_so() {
        let a = canon("m", serde_json::json!({"system": "a"}));
        let b = canon("m", serde_json::json!({"system": "b"}));
        let guess = RequestDiff::build(&b, &a, fps(), BaselineBy::Prefix).render(Fold::Summary, None);
        assert!(guess.contains("A GUESS"), "prefix baselines must be flagged:\n{guess}");

        let known = RequestDiff::build(&b, &a, fps(), BaselineBy::Label).render(Fold::Summary, None);
        assert!(!known.contains("A GUESS"), "a label baseline is not a guess:\n{known}");
    }

    /// tars names only the shapes it owns; anything else stays unnamed rather
    /// than being given an invented component.
    #[test]
    fn an_unknown_path_is_left_unnamed_not_guessed() {
        let a = canon("m", serde_json::json!({"vendor_extension": {"k": 1}}));
        let b = canon("m", serde_json::json!({"vendor_extension": {"k": 2}}));
        let d = RequestDiff::build(&b, &a, fps(), BaselineBy::Label);
        assert_eq!(d.changes[0].component, None);
        assert_eq!(d.changes[0].path, "/vendor_extension/k");
    }

    /// The provider→harness seam: facts cross the boundary, judgement does not.
    #[test]
    fn from_miss_reads_the_providers_facts() {
        let err = tars_types::ProviderError::CassetteMiss {
            want_fp: "w".into(),
            want_canon: canon("m", serde_json::json!({"system": "new"})),
            baseline_fp: Some("b".into()),
            baseline_canon: Some(canon("m", serde_json::json!({"system": "old"}))),
            baseline_selected_by: Some("label".into()),
        };
        let d = RequestDiff::from_miss(&err).expect("a baseline was captured");
        assert_eq!(d.baseline_selected_by, BaselineBy::Label);
        assert_eq!(d.fingerprint.baseline, "b");
        assert_eq!(d.changes[0].path, "/system");
        assert_eq!(d.changes[0].old.as_deref(), Some("old"));
    }

    /// Nothing captured ⇒ no diff. Returning an empty one would read as
    /// "nothing changed", which is the opposite of the truth.
    #[test]
    fn from_miss_without_a_captured_baseline_is_none() {
        let err = tars_types::ProviderError::CassetteMiss {
            want_fp: "w".into(),
            want_canon: "x".into(),
            baseline_fp: None,
            baseline_canon: None,
            baseline_selected_by: None,
        };
        assert!(RequestDiff::from_miss(&err).is_none());
        // A non-miss error is not a diff at all.
        assert!(RequestDiff::from_miss(&tars_types::ProviderError::Internal("x".into())).is_none());
    }

    /// An unrecognised selection basis degrades to "guess", never to "trusted":
    /// false confidence in the baseline is the costlier error.
    #[test]
    fn an_unknown_selection_basis_degrades_to_guess() {
        let err = tars_types::ProviderError::CassetteMiss {
            want_fp: "w".into(),
            want_canon: canon("m", serde_json::json!({"system": "new"})),
            baseline_fp: Some("b".into()),
            baseline_canon: Some(canon("m", serde_json::json!({"system": "old"}))),
            baseline_selected_by: Some("some-future-scheme".into()),
        };
        let d = RequestDiff::from_miss(&err).expect("baseline present");
        assert_eq!(d.baseline_selected_by, BaselineBy::Prefix);
        assert!(d.render(Fold::Summary, None).contains("A GUESS"));
    }

    #[test]
    fn added_and_removed_are_distinguished_from_changed() {
        let a = canon("m", serde_json::json!({"tools": [{"name": "read"}]}));
        let b = canon("m", serde_json::json!({"tools": [{"name": "read"}, {"name": "write"}]}));
        let d = RequestDiff::build(&b, &a, fps(), BaselineBy::Label);
        assert_eq!(d.summary.added, 1, "{:?}", d.changes);
        assert_eq!(d.changes[0].op, Op::Added);
        assert_eq!(d.changes[0].component.as_deref(), Some("tool-specs"));
    }

    /// The model participates in the fingerprint, so a model swap must show up
    /// as a change rather than as an unexplained miss.
    #[test]
    fn a_model_change_is_reported() {
        let body = serde_json::json!({"system": "s"});
        let d = RequestDiff::build(&canon("m2", body.clone()), &canon("m1", body), fps(), BaselineBy::Label);
        assert_eq!(d.changes.len(), 1);
        assert_eq!(d.changes[0].path, "/model");
        assert_eq!(d.changes[0].old.as_deref(), Some("m1"));
    }
}
