//! [`OutputLimit`] — the most output tokens a bound `provider + model` will
//! produce, and where that number came from.
//!
//! A request that sets no `max_output_tokens` does NOT mean "no limit": every
//! wire protocol then applies the provider's own default (DeepSeek: 8K in
//! non-thinking mode, against a 384K ceiling). So the service fills an unset
//! request with the model's real ceiling, and records WHICH ceiling it used —
//! the number alone can't say whether it was configured, measured from the
//! provider's API, or transcribed from the shipped model table.

use serde::{Deserialize, Serialize};

/// The output ceiling of one `provider + model`, decided when the service
/// binds them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutputLimit {
    /// The provider's config pins it (`max_output_tokens` under the
    /// provider block) — the user's number wins over any model table.
    Configured { tokens: u32 },
    /// The model's own ceiling, from the model catalog.
    Model { tokens: u32, source: CatalogSource },
    /// Nothing tars knows states this model's ceiling (a local model, a model
    /// the catalog has no row for, a provider built outside config). The
    /// request goes out without one and the provider's default applies.
    Unknown,
}

impl OutputLimit {
    /// The ceiling in tokens, when one is known.
    pub fn tokens(&self) -> Option<u32> {
        match self {
            Self::Configured { tokens } | Self::Model { tokens, .. } => Some(*tokens),
            Self::Unknown => None,
        }
    }
}

/// Where a model-catalog fact (a limit, the id list `@latest` picked from) came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum CatalogSource {
    /// Reported by the provider's list-models API during `tars models update`.
    Live {
        /// RFC3339 time of that query.
        queried_at: String,
    },
    /// Transcribed into the shipped `data/provider.toml` from the provider's
    /// docs.
    Shipped {
        /// The file's `verified` date.
        verified: String,
    },
}

impl std::fmt::Display for OutputLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Configured { tokens } => write!(f, "{tokens} (from provider config)"),
            Self::Model {
                tokens,
                source: CatalogSource::Live { queried_at },
            } => write!(f, "{tokens} (model max, provider API at {queried_at})"),
            Self::Model {
                tokens,
                source: CatalogSource::Shipped { verified },
            } => write!(f, "{tokens} (model max, provider.toml verified {verified})"),
            Self::Unknown => write!(f, "unknown — not sent, the provider's default applies"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_with_its_source() {
        let l = OutputLimit::Model {
            tokens: 65_536,
            source: CatalogSource::Live {
                queried_at: "2026-09-14T00:00:00Z".into(),
            },
        };
        let json = serde_json::to_value(&l).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "kind": "model",
                "tokens": 65536,
                "source": { "from": "live", "queried_at": "2026-09-14T00:00:00Z" }
            })
        );
        assert_eq!(serde_json::from_value::<OutputLimit>(json).unwrap(), l);
        assert_eq!(OutputLimit::Unknown.tokens(), None);
    }
}
