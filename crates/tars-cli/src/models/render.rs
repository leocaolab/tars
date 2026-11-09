//! Prints what `tars models` found: the refresh report of `update` and the listing, as text or
//! as one JSON object, on stdout.

use std::path::Path;

use tars_config::model_library::{Failure, LastQuery, Listing, ProviderEntry};

use super::{Finding, ListingRow, Previous, ProviderRefresh, STALE_AFTER_DAYS};

pub(super) fn print_update_human(path: &Path, changes: &[ProviderRefresh], findings: &[Finding]) {
    println!("Updated model library: {}\n", path.display());

    for c in changes {
        match &c.entry.last_query {
            LastQuery::Listed(listing) => {
                println!(
                    "  {}  ({}) — {} models",
                    c.name,
                    c.entry.provider_type,
                    listing.models.len()
                );
                if !c.added.is_empty() {
                    println!("      + added:   {}", c.added.join(", "));
                }
                if !c.removed.is_empty() {
                    println!(
                        "      - removed: {}  (deprecated/retired)",
                        c.removed.join(", ")
                    );
                }
                for l in &c.limit_changes {
                    println!(
                        "      ~ {} {}: {} → {}",
                        l.model,
                        l.field.label(),
                        fmt_opt(l.old),
                        fmt_opt(l.new)
                    );
                }
                if c.added.is_empty() && c.removed.is_empty() && c.limit_changes.is_empty() {
                    println!("      (no change)");
                }
            }
            LastQuery::NotAsked {
                note: reason, kept, ..
            }
            | LastQuery::Failed {
                failure: Failure { reason, .. },
                kept,
            } => {
                println!(
                    "  {}  ({}) — skipped: {}",
                    c.name, c.entry.provider_type, reason
                );
                if let Some(kept) = kept {
                    println!(
                        "      keeping the {} models listed at {}",
                        kept.models.len(),
                        kept.queried_at
                    );
                }
            }
        }
    }

    let (resolved, notes): (Vec<&Finding>, Vec<&Finding>) = findings.iter().partition(|f| {
        matches!(
            f,
            Finding::DefaultResolved {
                previous: Previous::Unchanged,
                ..
            }
        )
    });
    if !resolved.is_empty() {
        println!("\nDefaults:");
        for f in resolved {
            println!("  {f}");
        }
    }
    if !notes.is_empty() {
        println!("\n⚠ needs attention (config and provider.toml are not auto-edited):");
        for f in notes {
            println!("  {f}");
        }
    }
}

fn fmt_opt(n: Option<u64>) -> String {
    n.map_or_else(|| "not reported".to_string(), |n| n.to_string())
}

pub(super) fn print_list_human(
    live: bool,
    updated_at: &str,
    age_days: Option<Result<i64, chrono::ParseError>>,
    rows: &[ListingRow],
) {
    if live {
        println!("Models (live):\n");
    } else {
        println!("Models (library, updated {updated_at}):");
        match age_days {
            Some(Ok(days)) if days >= STALE_AFTER_DAYS => println!(
                "⚠ the library is {days} days old — run `tars models update` (new models and \
                 limits reach the runtime only through a refresh)"
            ),
            Some(Err(e)) => println!(
                "⚠ the library's timestamp `{updated_at}` is unreadable ({e}) — run \
                 `tars models update` to rewrite it"
            ),
            Some(Ok(_)) | None => {}
        }
        println!();
    }
    for row in rows {
        let Some(entry) = &row.entry else {
            println!(
                "  {}  ({})  [default: {}] — not in library — run `tars models update`",
                row.name,
                row.provider_type,
                row.default_label(),
            );
            continue;
        };
        match &entry.last_query {
            LastQuery::Listed(listing) => print_listed_row(row, entry, listing, None),
            LastQuery::NotAsked {
                note,
                kept: Some(listing),
                ..
            } => print_listed_row(
                row,
                entry,
                listing,
                Some(format!(
                    "the last update did not ask: {note} — listing the models from {}",
                    listing.queried_at
                )),
            ),
            LastQuery::Failed {
                failure,
                kept: Some(listing),
            } => print_listed_row(
                row,
                entry,
                listing,
                Some(format!(
                    "the query at {} failed: {} — listing the models from {}",
                    failure.queried_at, failure.reason, listing.queried_at
                )),
            ),
            LastQuery::NotAsked {
                note: reason,
                kept: None,
                ..
            }
            | LastQuery::Failed {
                failure: Failure { reason, .. },
                kept: None,
            } => println!(
                "  {}  ({})  [default: {}] — {}",
                row.name,
                entry.provider_type,
                row.default_label(),
                reason
            ),
        }
    }
}

/// Prints a provider row with its live list; `warning` says why the last update listed nothing
/// after `listing` was taken.
fn print_listed_row(
    row: &ListingRow,
    entry: &ProviderEntry,
    listing: &Listing,
    warning: Option<String>,
) {
    let stale = if row.default_listed == Some(false) {
        "  ⚠ default not in list (stale config?)"
    } else {
        ""
    };
    println!(
        "  {}  ({})  [default: {}]{}",
        row.name,
        entry.provider_type,
        row.default_label(),
        stale
    );
    if let Some(warning) = warning {
        println!("      ⚠ {warning}");
    }
    if listing.models.is_empty() {
        println!("      (no models listed)");
    }
    for m in &listing.models {
        let marker = if row.default_resolved.as_deref() == Ok(m.id.as_str()) {
            "  ← default"
        } else {
            ""
        };
        let limits = match (m.context, m.max_output) {
            (None, None) => String::new(),
            (c, o) => format!("  (context {}, max output {})", fmt_opt(c), fmt_opt(o)),
        };
        println!("      {}{limits}{marker}", m.id);
    }
}

pub(super) fn print_list_json(
    live: bool,
    updated_at: &str,
    rows: &[ListingRow],
) -> Result<(), serde_json::Error> {
    let providers: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let last_query = r.entry.as_ref().map(|e| &e.last_query);
            let (status, note) = match last_query {
                None => (serde_json::json!("not_in_library"), None),
                Some(LastQuery::Listed(_)) => (serde_json::json!("ok"), None),
                Some(LastQuery::NotAsked { why, note, .. }) => (serde_json::json!(why), Some(note)),
                Some(LastQuery::Failed { failure, .. }) => {
                    (serde_json::json!(failure.kind), Some(&failure.reason))
                }
            };
            let listing = last_query.and_then(LastQuery::listing);
            serde_json::json!({
                "name": r.name,
                "type": r.entry.as_ref().map_or(r.provider_type, |e| e.provider_type),
                "catalog": r.entry.as_ref().map_or(r.catalog.as_str(), |e| e.catalog.as_str()),
                "status": status,
                "note": note,
                "default_model": r.default_spec,
                "default_resolved": r.default_resolved.as_ref().ok(),
                "default_error": r.default_resolved.as_ref().err(),
                "default_listed": r.default_listed,
                "models": listing.map_or(&[][..], |l| &l.models[..]),
                "listed_at": listing.map(|l| &l.queried_at),
                "queried_at": last_query.and_then(LastQuery::queried_at),
            })
        })
        .collect();
    let env = serde_json::json!({
        "command": "models",
        "source": if live { "live" } else { "library" },
        "updated_at": updated_at,
        "providers": providers,
    });
    println!("{}", serde_json::to_string_pretty(&env)?);
    Ok(())
}
