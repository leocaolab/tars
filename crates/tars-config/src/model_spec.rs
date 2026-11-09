//! Parses what a config or caller writes where a model id goes.
//!
//! A spec is either a concrete provider-side id (`gemini-3.8-flash`,
//! `qwen/qwen3-coder-30b`), passed through untouched, or `<series>@latest`
//! (`flash@latest`), resolved against a provider's model list to the newest id
//! in that series. The series grammar is per-provider DATA
//! (`[providers.X.series]` in `data/provider.toml`): a pattern with one
//! `{version}` slot.
//!
//! Resolution happens once, when a model is bound to a provider, so everything
//! downstream (events, cassette fingerprints) sees the concrete id
//! that actually ran — never the moving `@latest`.

use thiserror::Error;

use tars_types::CatalogSource;

/// The only version keyword a spec accepts.
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
    /// Parses a spec; any `@` makes it a series spec, and the part after it must be `latest`.
    /// Whitespace around the spec is not part of it, as no provider names a model with it; a
    /// blank spec names no model and is refused, so no provider is sent one.
    pub fn parse(raw: &'a str) -> Result<Self, ParseModelSpecError> {
        let spec = raw.trim();
        if spec.is_empty() {
            return Err(ParseModelSpecError::Blank {
                spec: raw.to_string(),
            });
        }
        match spec.split_once('@') {
            None => Ok(Self::Concrete(spec)),
            Some((series, LATEST)) if !series.is_empty() => Ok(Self::Latest { series }),
            Some(("", _)) => Err(ParseModelSpecError::NoSeriesBeforeAt {
                spec: spec.to_string(),
            }),
            Some((series, version)) => Err(ParseModelSpecError::UnsupportedVersion {
                series: series.to_string(),
                version: version.to_string(),
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

/// Says why a spec does not parse.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ParseModelSpecError {
    #[error("the model spec {spec:?} is blank; name a model id or `<series>@latest`")]
    Blank { spec: String },
    #[error("model spec `{spec}`: no series before `@` (write e.g. `flash@latest`)")]
    NoSeriesBeforeAt { spec: String },
    #[error(
        "model spec `{series}@{version}`: version `{version}` is not supported — only \
         `{series}@latest` is"
    )]
    UnsupportedVersion { series: String, version: String },
}

/// Says why a spec did not resolve, naming the provider, the spec and what was searched so the
/// message says what to fix.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ResolveModelSpecError {
    #[error(transparent)]
    Parse(#[from] ParseModelSpecError),
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
         {} ({candidates} ids searched)",
        list_searched(searched)
    )]
    NoMatch {
        provider: String,
        series: String,
        pattern: Box<SeriesPattern>,
        searched: CatalogSource,
        candidates: usize,
    },
}

/// Returns how a message names the id list a spec was resolved against.
fn list_searched(searched: &CatalogSource) -> String {
    match searched {
        CatalogSource::Live { queried_at } => format!("live model list from {queried_at}"),
        CatalogSource::Shipped { verified } => {
            format!("shipped provider.toml (verified {verified})")
        }
    }
}

/// A series pattern (`gemini-{version}-flash`) split around its one `{version}` slot. The data
/// file's patterns are parsed when it loads, so a pattern without exactly one slot never reaches
/// a resolution.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct SeriesPattern {
    prefix: String,
    suffix: String,
}

impl TryFrom<String> for SeriesPattern {
    type Error = ParseSeriesPatternError;

    fn try_from(pattern: String) -> Result<Self, Self::Error> {
        Self::parse(&pattern).ok_or(ParseSeriesPatternError { pattern })
    }
}

/// Says that a series pattern does not hold exactly one `{version}` slot.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("series pattern `{pattern}` needs exactly one {{version}}")]
pub struct ParseSeriesPatternError {
    pattern: String,
}

impl std::fmt::Display for SeriesPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{VERSION_SLOT}{}", self.prefix, self.suffix)
    }
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

impl SeriesPattern {
    /// Splits a pattern at its single `{version}` slot; returns `None` when the slot is missing
    /// or repeated.
    pub fn parse(pattern: &str) -> Option<Self> {
        let (prefix, suffix) = pattern.split_once(VERSION_SLOT)?;
        if suffix.contains(VERSION_SLOT) {
            return None;
        }
        Some(Self {
            prefix: prefix.to_string(),
            suffix: suffix.to_string(),
        })
    }

    /// Ranks `id` within this series, or returns `None` if it is not a member. The version is
    /// dot-separated numbers; one trailing `-preview` is allowed.
    pub fn rank(&self, id: &str) -> Option<SeriesRank> {
        let (base, ga) = match id.strip_suffix(PREVIEW_SUFFIX) {
            Some(base) => (base, false),
            None => (id, true),
        };
        let version = base
            .strip_prefix(self.prefix.as_str())?
            .strip_suffix(self.suffix.as_str())?;
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

    /// Returns the newest member of this series among `ids`.
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
    fn a_blank_spec_is_refused_and_whitespace_around_an_id_is_not_part_of_it() {
        assert_eq!(
            ModelSpec::parse(""),
            Err(ParseModelSpecError::Blank {
                spec: String::new()
            })
        );
        assert_eq!(
            ModelSpec::parse(" \t"),
            Err(ParseModelSpecError::Blank { spec: " \t".into() })
        );
        assert_eq!(ModelSpec::parse("m"), Ok(ModelSpec::Concrete("m")));
        assert_eq!(ModelSpec::parse(" m\n"), Ok(ModelSpec::Concrete("m")));
        assert_eq!(
            ModelSpec::parse(" flash@latest "),
            Ok(ModelSpec::Latest { series: "flash" })
        );
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
