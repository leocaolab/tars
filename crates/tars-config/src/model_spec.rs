//! Model specs — what a config or caller writes where a model id goes.
//!
//! A spec is either a concrete provider-side id (`gemini-3.8-flash`,
//! `qwen/qwen3-coder-30b`), passed through untouched, or `<series>@latest`
//! (`flash@latest`), resolved against a provider's model list to the newest id
//! in that series. The series grammar is per-provider DATA
//! (`[providers.X.series]` in `data/provider.toml`): a pattern with one
//! `{version}` slot.
//!
//! Resolution happens once, when a model is bound to a provider, so everything
//! downstream (events, cache keys, cassette fingerprints) sees the concrete id
//! that actually ran — never the moving `@latest`.

use thiserror::Error;

use tars_types::CatalogSource;

/// The only version keyword a spec accepts today.
const LATEST: &str = "latest";

/// A parsed model spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelSpec<'a> {
    /// A concrete id, used as written.
    Concrete(&'a str),
    /// `<series>@latest` — the newest id in `series`.
    Latest { series: &'a str },
}

impl<'a> ModelSpec<'a> {
    /// Parse a spec. Any `@` makes it a series spec; the part after it must be
    /// `latest`.
    pub fn parse(spec: &'a str) -> Result<Self, ModelSpecError> {
        match spec.split_once('@') {
            None => Ok(Self::Concrete(spec)),
            Some((series, LATEST)) if !series.is_empty() => Ok(Self::Latest { series }),
            Some((series, version)) => Err(ModelSpecError::BadSpec {
                spec: spec.to_string(),
                detail: if series.is_empty() {
                    "no series before `@` (write e.g. `flash@latest`)".to_string()
                } else {
                    format!("version `{version}` is not supported — only `{series}@latest` is")
                },
            }),
        }
    }
}

/// A model spec resolved to a concrete id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedModel {
    /// The concrete provider-side id to call.
    pub id: String,
    /// For a `@latest` spec, the id list it was picked from; `None` when the
    /// spec was already concrete.
    pub picked_from: Option<CatalogSource>,
}

/// Why a spec did not resolve. Carries the provider, the spec and what was
/// searched, so the message says what to fix.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ModelSpecError {
    #[error("model spec `{spec}`: {detail}")]
    BadSpec { spec: String, detail: String },
    #[error(
        "model spec `{series}@latest`: provider `{provider}` defines no model series in \
         data/provider.toml — use a concrete model id"
    )]
    NoSeries { provider: String, series: String },
    #[error(
        "model spec `{series}@latest`: provider `{provider}` has no series `{series}` \
         (known: {})",
        known.join(", ")
    )]
    UnknownSeries {
        provider: String,
        series: String,
        known: Vec<String>,
    },
    #[error(
        "model spec `{series}@latest`: no {provider} model id matches `{pattern}` in the \
         {searched} ({candidates} ids searched)"
    )]
    NoMatch {
        provider: String,
        series: String,
        pattern: String,
        /// Human description of the list searched.
        searched: String,
        candidates: usize,
    },
}

/// A series pattern (`gemini-{version}-flash`) split around its `{version}`
/// slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeriesPattern<'a> {
    prefix: &'a str,
    suffix: &'a str,
}

/// Where a matched id sits in its series: its version, and whether it is a
/// `-preview`. Ordered so the max is the one `@latest` picks — higher version
/// first, then GA over preview.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SeriesRank {
    version: Vec<u32>,
    ga: bool,
}

const VERSION_SLOT: &str = "{version}";
const PREVIEW_SUFFIX: &str = "-preview";

impl<'a> SeriesPattern<'a> {
    /// Split a pattern at its single `{version}` slot. `None` when the slot is
    /// missing or repeated — an authoring bug the KB test catches.
    pub fn parse(pattern: &'a str) -> Option<Self> {
        let (prefix, suffix) = pattern.split_once(VERSION_SLOT)?;
        if suffix.contains(VERSION_SLOT) {
            return None;
        }
        Some(Self { prefix, suffix })
    }

    /// Rank `id` within this series, or `None` if it is not a member. The
    /// version is dot-separated numbers; one trailing `-preview` is allowed.
    pub fn rank(&self, id: &str) -> Option<SeriesRank> {
        let (base, ga) = match id.strip_suffix(PREVIEW_SUFFIX) {
            Some(base) => (base, false),
            None => (id, true),
        };
        let version = base.strip_prefix(self.prefix)?.strip_suffix(self.suffix)?;
        if version.is_empty() {
            return None;
        }
        let version = version
            .split('.')
            .map(|part| {
                (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
                    .then(|| part.parse::<u32>().ok())
                    .flatten()
            })
            .collect::<Option<Vec<u32>>>()?;
        Some(SeriesRank { version, ga })
    }

    /// The newest member of this series among `ids`.
    pub fn latest<'i>(&self, ids: impl IntoIterator<Item = &'i str>) -> Option<&'i str> {
        ids.into_iter()
            .filter_map(|id| self.rank(id).map(|r| (r, id)))
            .max()
            .map(|(_, id)| id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_spec() {
        assert_eq!(
            ModelSpec::parse("gemini-3.8-flash").unwrap(),
            ModelSpec::Concrete("gemini-3.8-flash")
        );
        assert_eq!(
            ModelSpec::parse("flash@latest").unwrap(),
            ModelSpec::Latest { series: "flash" }
        );
        let err = ModelSpec::parse("flash@3.7").unwrap_err();
        assert!(err.to_string().contains("only `flash@latest`"), "{err}");
        assert!(ModelSpec::parse("@latest").is_err());
    }

    #[test]
    fn rank_accepts_only_exact_series_members() {
        let flash = SeriesPattern::parse("gemini-{version}-flash").unwrap();
        assert!(flash.rank("gemini-3.8-flash").is_some());
        assert!(flash.rank("gemini-3-flash-preview").is_some());
        assert!(flash.rank("gemini-3.5-flash-lite").is_none());
        assert!(flash.rank("gemini-2.5-flash-preview-tts").is_none());
        assert!(flash.rank("gemini-flash-latest").is_none());
        assert!(flash.rank("gemini-3.1-flash-image").is_none());
        assert!(flash.rank("gemini-x.1-flash").is_none());

        let pro = SeriesPattern::parse("gemini-{version}-pro").unwrap();
        assert!(pro.rank("gemini-3.1-pro-preview").is_some());
        assert!(pro.rank("gemini-3.1-pro-preview-customtools").is_none());
    }

    #[test]
    fn latest_is_highest_version_then_ga_over_preview() {
        let flash = SeriesPattern::parse("gemini-{version}-flash").unwrap();
        let ids = [
            "gemini-2.5-flash",
            "gemini-3-flash-preview",
            "gemini-3.5-flash",
            "gemini-3.10-flash-preview",
            "gemini-3.8-flash",
            "gemini-3.5-flash-lite",
        ];
        // 3.10 > 3.8 numerically, not lexically; a newer preview beats an
        // older GA.
        assert_eq!(flash.latest(ids), Some("gemini-3.10-flash-preview"));
        assert_eq!(
            flash.latest(["gemini-3.8-flash-preview", "gemini-3.8-flash"]),
            Some("gemini-3.8-flash")
        );

        let pro = SeriesPattern::parse("gemini-{version}-pro").unwrap();
        assert_eq!(
            pro.latest(["gemini-2.5-pro", "gemini-3.1-pro-preview"]),
            Some("gemini-3.1-pro-preview")
        );
        assert_eq!(pro.latest(["gemini-3.8-flash"]), None);
    }

    #[test]
    fn pattern_needs_exactly_one_slot() {
        assert!(SeriesPattern::parse("gemini-flash").is_none());
        assert!(SeriesPattern::parse("{version}-{version}").is_none());
    }
}
