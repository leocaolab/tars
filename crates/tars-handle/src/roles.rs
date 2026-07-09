//! Role → provider resolution (Doc 06 §6 C3), as standalone functions.
//!
//! Extracted from the `Tars` handle so an embedder (arc/concer, phase 2) can
//! resolve a role against an explicit registry + routing + roles map WITHOUT
//! any handle / scope machinery. No global, no scope, no hidden state — every
//! input is a plain argument. The `Tars` handle itself delegates to these.

use std::collections::HashMap;
use std::sync::Arc;

use tars_config::RoutingConfig;
use tars_provider::{LlmProvider, ProviderRegistry};
use tars_types::{ModelTier, ProviderId};

use crate::error::TarsError;

/// Resolve `role` → `(provider id, provider)` against an explicit registry.
///
/// Resolution order (highest priority first):
/// 1. the flat `[roles]` map (`role` → provider id) — the shape arc/concer write;
/// 2. `role` naming a fixed tier → that tier's first candidate;
/// 3. `role` as a literal provider id;
/// 4. the `default` tier's first candidate;
/// 5. the sole provider if the registry has exactly one;
///
/// else [`TarsError::UnknownRole`].
pub fn resolve_role(
    roles: &HashMap<String, ProviderId>,
    routing: &RoutingConfig,
    registry: &ProviderRegistry,
    role: &str,
) -> Result<(ProviderId, Arc<dyn LlmProvider>), TarsError> {
    // 1. flat `[roles]` map: arbitrary name → provider id. Highest priority.
    if let Some(id) = roles.get(role) {
        if let Some(p) = registry.get(id) {
            return Ok((id.clone(), p));
        }
    }
    // 2. role names a tier → first candidate in that tier.
    if let Some(tier) = parse_tier(role) {
        if let Some(hit) = first_in_tier(routing, registry, &tier) {
            return Ok(hit);
        }
    }
    // 3. role is a literal provider id.
    let literal = ProviderId::new(role);
    if let Some(p) = registry.get(&literal) {
        return Ok((literal, p));
    }
    // 4. fall back to the `default` tier.
    if let Some(hit) = first_in_tier(routing, registry, &ModelTier::Default) {
        return Ok(hit);
    }
    // 5. a single-provider registry has an unambiguous answer.
    if registry.len() == 1 {
        if let Some(id) = registry.ids().next().cloned() {
            if let Some(p) = registry.get(&id) {
                return Ok((id, p));
            }
        }
    }
    Err(TarsError::UnknownRole {
        role: role.to_string(),
        tried: Some(literal),
    })
}

/// Like [`resolve_role`] but returns only the resolved provider id (no
/// registry lookup for the live provider).
pub fn resolve_provider_id(
    roles: &HashMap<String, ProviderId>,
    routing: &RoutingConfig,
    registry: &ProviderRegistry,
    role: &str,
) -> Result<ProviderId, TarsError> {
    resolve_role(roles, routing, registry, role).map(|(id, _)| id)
}

fn first_in_tier(
    routing: &RoutingConfig,
    registry: &ProviderRegistry,
    tier: &ModelTier,
) -> Option<(ProviderId, Arc<dyn LlmProvider>)> {
    let id = routing.tiers.get(tier)?.first()?;
    let provider = registry.get(id)?;
    Some((id.clone(), provider))
}

/// Map a role string to a fixed [`ModelTier`] (case-insensitive), or `None`
/// when the role is not one of the four tier names. This is only the *tier*
/// fallback in [`resolve_role`]: an arbitrary role name is served first by the
/// flat `[roles]` map.
pub fn parse_tier(role: &str) -> Option<ModelTier> {
    match role.to_ascii_lowercase().as_str() {
        "reasoning" => Some(ModelTier::Reasoning),
        "default" => Some(ModelTier::Default),
        "fast" => Some(ModelTier::Fast),
        "local" => Some(ModelTier::Local),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tars_config::RoutingConfig;
    use tars_provider::MockProvider;
    use tars_provider::backends::mock::CannedResponse;

    /// A registry holding one `mock` provider per id (DI seam, no config).
    fn registry(ids: &[&str]) -> ProviderRegistry {
        let mut map: HashMap<ProviderId, Arc<dyn LlmProvider>> = HashMap::new();
        for id in ids {
            let pid = ProviderId::new(*id);
            map.insert(
                pid.clone(),
                MockProvider::new(pid, CannedResponse::Text("hi".into())),
            );
        }
        ProviderRegistry::from_providers(map)
    }

    fn roles(pairs: &[(&str, &str)]) -> HashMap<String, ProviderId> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), ProviderId::new(*v)))
            .collect()
    }

    fn routing(default: Option<&str>) -> RoutingConfig {
        let mut tiers = HashMap::new();
        if let Some(id) = default {
            tiers.insert(ModelTier::Default, vec![ProviderId::new(id)]);
        }
        RoutingConfig { tiers }
    }

    #[test]
    fn flat_roles_map_wins_over_the_tier_fallback() {
        // `critic` maps to `mock2` even though the `default` tier is `mock1`.
        let (r, rt, reg) = (
            roles(&[("critic", "mock2")]),
            routing(Some("mock1")),
            registry(&["mock1", "mock2"]),
        );
        let (id, _) = resolve_role(&r, &rt, &reg, "critic").expect("flat entry resolves");
        assert_eq!(id, ProviderId::new("mock2"));
    }

    #[test]
    fn tier_literal_and_default_fallbacks() {
        let (r, rt, reg) = (
            HashMap::new(),
            routing(Some("mock1")),
            registry(&["mock1", "mock2"]),
        );
        // (a) role names the `default` tier.
        assert_eq!(
            resolve_provider_id(&r, &rt, &reg, "default").unwrap(),
            ProviderId::new("mock1"),
        );
        // (b) role is a literal provider id.
        assert_eq!(
            resolve_provider_id(&r, &rt, &reg, "mock2").unwrap(),
            ProviderId::new("mock2"),
        );
        // (c) unknown role falls through to the `default` tier candidate.
        assert_eq!(
            resolve_provider_id(&r, &rt, &reg, "whatever").unwrap(),
            ProviderId::new("mock1"),
        );
    }

    #[test]
    fn sole_provider_absorbs_any_unmapped_role() {
        // One provider, no roles, no tiers → rule 5 answers for any role.
        let (r, rt, reg) = (HashMap::new(), routing(None), registry(&["only"]));
        assert_eq!(
            resolve_provider_id(&r, &rt, &reg, "some_arbitrary_role").unwrap(),
            ProviderId::new("only"),
        );
    }

    #[test]
    fn unmapped_role_errors_unknown_role() {
        // Two providers, no roles, no tiers → no fallback can absorb it.
        let (r, rt, reg) = (HashMap::new(), routing(None), registry(&["mock1", "mock2"]));
        match resolve_role(&r, &rt, &reg, "nonexistent_role") {
            Err(TarsError::UnknownRole { role, .. }) => assert_eq!(role, "nonexistent_role"),
            Ok(_) => panic!("unmapped role must not resolve"),
        }
    }
}
