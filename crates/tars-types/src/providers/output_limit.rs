//! Records the most output tokens a bound `provider + model` will produce, and where that number
//! came from.
//!
//! A request with no `max_output_tokens` is not unlimited: the provider applies its own default
//! (DeepSeek: 8K in non-thinking mode, against a 384K ceiling). So the service fills an unset
//! request with the model's ceiling and records which ceiling it used, since the number alone
//! does not say whether it was configured, queried from the provider or shipped in the table.

use serde::{Deserialize, Serialize};

/// The output ceiling of one `provider + model`, decided when the service
/// binds them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutputLimit {
    /// Set by `max_output_tokens` in the provider's config block, which wins over any model table.
    Configured { tokens: u32 },
    /// The model's own ceiling from the model catalog.
    Model { tokens: u32, source: CatalogSource },
    /// Nothing tars knows states the ceiling (a local model, a model with no catalog row, a
    /// provider built outside config), so the request goes out without one and the provider's
    /// default applies.
    Unknown,
}

impl OutputLimit {
    /// Returns the ceiling in tokens, when one is known.
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
