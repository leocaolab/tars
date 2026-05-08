//! Routing config — Doc 01 §12 + Doc 02 §4.6 wired through TOML.
//!
//! M2 ships only `tiers` — the `ModelTier → Vec<ProviderId>` lookup
//! [`tars_pipeline::TierPolicy`] consumes. Cost / Latency / Ensemble
//! policies don't need config (Cost/Latency derive from runtime
//! metrics; Ensemble is invoked per-request via `ModelHint::Ensemble`).
//!
//! TOML shape:
//!
//! ```toml
//! [routing.tiers]
//! reasoning = ["anthropic_main", "openai_o1"]
//! default   = ["openai_main", "anthropic_main"]
//! fast      = ["gemini_flash", "openai_mini"]
//! local     = ["mlx_local", "llamacpp_local"]
//! ```

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use tars_types::{ModelTier, ProviderId};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingConfig {
    /// Ordered candidate list per tier. Order = priority.
    /// Empty / missing tier → no candidates → routing returns
    /// `InvalidRequest` (caller asked for a tier nothing's wired to).
    #[serde(default)]
    pub tiers: HashMap<ModelTier, Vec<ProviderId>>,
}

impl RoutingConfig {
    pub fn is_empty(&self) -> bool {
        self.tiers.is_empty()
    }

    /// Validate references against a known set of provider IDs. The
    /// loader calls this after building [`super::ProvidersConfig`] so
    /// dangling references (`tier = ["typo_id"]` with no
    /// corresponding `[providers.typo_id]`) get caught at startup.
    pub fn validate(
        &self,
        known_providers: &std::collections::HashSet<ProviderId>,
        sink: &mut Vec<crate::error::ValidationError>,
    ) {
        for (tier, candidates) in &self.tiers {
            if candidates.is_empty() {
                sink.push(crate::error::ValidationError::new(
                    format!("routing.tiers.{tier:?}").to_lowercase(),
                    "tier candidate list is empty — drop the entry or add a provider",
                ));
                continue;
            }
            let mut seen = std::collections::HashSet::new();
            for id in candidates {
                if !known_providers.contains(id) {
                    sink.push(crate::error::ValidationError::new(
                        format!("routing.tiers.{tier:?}").to_lowercase(),
                        format!(
                            "references unknown provider `{id}` — add a [providers.{id}] section or remove this entry"
                        ),
                    ));
                }
                if !seen.insert(id) {
                    sink.push(crate::error::ValidationError::new(
                        format!("routing.tiers.{tier:?}").to_lowercase(),
                        format!(
                            "duplicate provider `{id}` in candidate list — each provider should appear at most once per tier"
                        ),
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_through_toml() {
        let toml_str = r#"
            [tiers]
            reasoning = ["a", "b"]
            fast = ["c"]
        "#;
        let cfg: RoutingConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.tiers.len(), 2);
        assert_eq!(
            cfg.tiers.get(&ModelTier::Reasoning).unwrap(),
            &vec![ProviderId::new("a"), ProviderId::new("b")]
        );
        assert_eq!(
            cfg.tiers.get(&ModelTier::Fast).unwrap(),
            &vec![ProviderId::new("c")]
        );
        // Verify Serialize→Deserialize round-trips back to the same struct.
        let reserialized = toml::to_string(&cfg).unwrap();
        let cfg2: RoutingConfig = toml::from_str(&reserialized).unwrap();
        assert_eq!(cfg.tiers, cfg2.tiers);
    }

    #[test]
    fn empty_default() {
        let cfg = RoutingConfig::default();
        assert!(cfg.is_empty());
    }

    #[test]
    fn validate_flags_dangling_reference() {
        let mut tiers = HashMap::new();
        tiers.insert(
            ModelTier::Reasoning,
            vec![ProviderId::new("real_provider"), ProviderId::new("typo_id")],
        );
        let cfg = RoutingConfig { tiers };
        let mut known = std::collections::HashSet::new();
        known.insert(ProviderId::new("real_provider"));
        let mut errs = Vec::new();
        cfg.validate(&known, &mut errs);
        // Only the dangling reference produces an error — `real_provider`
        // must not be flagged, and the error must point at the right key.
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].key, "routing.tiers.reasoning");
        assert!(errs[0].message.contains("typo_id"));
        assert!(!errs[0].message.contains("real_provider"));
    }

    #[test]
    fn validate_flags_empty_candidate_list() {
        let mut tiers = HashMap::new();
        tiers.insert(ModelTier::Fast, vec![]);
        let cfg = RoutingConfig { tiers };
        let mut errs = Vec::new();
        cfg.validate(&std::collections::HashSet::new(), &mut errs);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].key, "routing.tiers.fast");
        assert!(errs[0].message.contains("empty"));
    }

    #[test]
    fn validate_flags_duplicate_provider() {
        let mut tiers = HashMap::new();
        tiers.insert(
            ModelTier::Default,
            vec![
                ProviderId::new("a"),
                ProviderId::new("a"),
                ProviderId::new("b"),
            ],
        );
        let cfg = RoutingConfig { tiers };
        let mut known = std::collections::HashSet::new();
        known.insert(ProviderId::new("a"));
        known.insert(ProviderId::new("b"));
        let mut errs = Vec::new();
        cfg.validate(&known, &mut errs);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].key, "routing.tiers.default");
        assert!(errs[0].message.contains("duplicate"));
        assert!(errs[0].message.contains('a'));
    }

    #[test]
    fn rejects_unknown_field() {
        let toml_str = r#"
            tiers = { fast = ["c"] }
            random_typo = "boom"
        "#;
        let r: Result<RoutingConfig, _> = toml::from_str(toml_str);
        let err = r.unwrap_err().to_string();
        // Verify rejection is specifically because of the unknown field,
        // not a syntax / type error masquerading as success.
        assert!(
            err.contains("random_typo") || err.contains("unknown field"),
            "expected unknown-field error, got: {err}",
        );
    }
}
