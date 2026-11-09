//! Checks which [`Reason`]s come back from a durable record as themselves and which come
//! back as [`Reason::Replayed`].

use tars_types::{Allowance, Audience, JournaledReason, Reason};

/// Sends `r` the way a durable store row takes it: to a record, to JSON, and back.
fn through_the_journal(r: &Reason) -> Reason {
    let stored = JournaledReason::from(r);
    let json = serde_json::to_string(&stored).expect("a record serialises");
    let read: JournaledReason = serde_json::from_str(&json).expect("and comes back");
    Reason::from(read)
}

#[test]
fn a_spent_allowance_comes_back_as_itself_with_its_ceiling() {
    let out = through_the_journal(&Reason::Exhausted {
        allowance: Allowance::Turns,
        limit: 12,
        last_words: Some("still checking the error paths".into()),
    });
    let Reason::Exhausted {
        allowance,
        limit,
        last_words,
    } = out
    else {
        panic!("a ceiling that ran out came back as something else: {out:?}");
    };
    assert_eq!(
        (allowance, limit),
        (Allowance::Turns, 12),
        "which ceiling, and how big"
    );
    assert_eq!(
        last_words.as_deref(),
        Some("still checking the error paths")
    );
}

#[test]
fn a_stall_comes_back_with_the_turns_it_stood_still_for() {
    let out = through_the_journal(&Reason::Stalled {
        turns: 3,
        last_words: Some("{\"tool\": ".into()),
    });
    let Reason::Stalled { turns, last_words } = out else {
        panic!("{out:?}")
    };
    assert_eq!(turns, 3);
    assert_eq!(last_words.as_deref(), Some("{\"tool\": "));
}

#[test]
fn a_panic_is_still_replayed_because_its_site_cannot_be_rebuilt() {
    let out = through_the_journal(&Reason::Panicked {
        site: "world::step",
        payload: Some("index out of bounds".into()),
    });
    let Reason::Replayed {
        class,
        handler,
        text,
        ..
    } = out
    else {
        panic!("a panic must not come back impersonating a concrete variant: {out:?}");
    };
    assert_eq!(
        class,
        Reason::Panicked {
            site: "world::step",
            payload: None
        }
        .error_class()
    );
    assert_eq!(handler, Audience::Runtime, "a defect never reaches a model");
    assert!(
        text.contains("index out of bounds"),
        "the producer's own words: {text}"
    );
}

#[test]
fn a_record_written_before_this_field_still_reads() {
    let json = r#"{"class":"permanent","handler":"runtime","text":"ran out of turns: the ceiling was 12","raw":null}"#;
    let old: JournaledReason = serde_json::from_str(json).expect("an older row still reads");
    assert_eq!(old.exact, None);
    assert!(matches!(Reason::from(old), Reason::Replayed { .. }));
}

#[test]
fn an_unresumable_task_comes_back_with_its_step_and_count() {
    let out = through_the_journal(&Reason::Unresumable { at: 7, times: 3 });
    let Reason::Unresumable { at, times } = out else {
        panic!("{out:?}")
    };
    assert_eq!((at, times), (7, 3));
}
