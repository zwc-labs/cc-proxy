//! Inbound model-name resolution + fallback observability.
//!
//! Resolution order (first hit wins):
//!
//! 1. profile / alias pass-through after stripping the `[1m]` context marker;
//! 2. exact `[models.mapping]` key;
//! 3. `[models.mapping]` key (or profile alias) after stripping structural
//!    suffixes: `-YYYYMMDD`, `-latest`, Bedrock-style `-vN:M`;
//! 4. `[models].default` — counted, and warned once per inbound name.
//!
//! Before this module existed, a dated model id such as
//! `claude-sonnet-4-5-20250929` missed every exact mapping key and degraded to
//! the default model with no signal at all. Resolution is now classified, so a
//! silent downgrade is visible in the log stream and on `/health`.

use crate::config::Config;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use tracing::{debug, warn};

/// How an inbound model name resolved to an upstream model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionKind {
    /// The inbound name is already a configured profile name or alias.
    Profile,
    /// Exact key hit in `[models.mapping]`.
    Mapping,
    /// Hit after stripping a structural suffix (`-YYYYMMDD`, `-latest`,
    /// `-vN:M`) from the inbound name.
    Normalized,
    /// Nothing matched; `[models].default` was used.
    Fallback,
}

impl ResolutionKind {
    /// Stable label used in logs and on `/health`.
    pub fn as_str(self) -> &'static str {
        match self {
            ResolutionKind::Profile => "profile",
            ResolutionKind::Mapping => "mapping",
            ResolutionKind::Normalized => "normalized",
            ResolutionKind::Fallback => "fallback",
        }
    }
}

/// The outcome of resolving one inbound model name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelResolution {
    /// Model name sent upstream.
    pub upstream: String,
    /// How `upstream` was chosen.
    pub kind: ResolutionKind,
    /// The normalized name that matched (only for [`ResolutionKind::Normalized`]).
    pub matched_alias: Option<String>,
}

impl ModelResolution {
    fn new(upstream: String, kind: ResolutionKind, matched_alias: Option<String>) -> Self {
        Self {
            upstream,
            kind,
            matched_alias,
        }
    }
}

/// Counters exposed on `/health` (process lifetime, not per-window).
static PROFILE_HITS: AtomicU64 = AtomicU64::new(0);
static MAPPING_HITS: AtomicU64 = AtomicU64::new(0);
static NORMALIZED_HITS: AtomicU64 = AtomicU64::new(0);
static FALLBACK_HITS: AtomicU64 = AtomicU64::new(0);

/// Bounded maps so an hostile/buggy client cannot grow process memory by
/// sending unbounded distinct model names.
const MAX_TRACKED_NAMES: usize = 64;

/// `inbound → matched suffix-free name` for normalized hits.
static NORMALIZED_MODELS: Mutex<Option<BTreeMap<String, String>>> = Mutex::new(None);
/// `inbound → hit count` for fallbacks.
static FALLBACK_MODELS: Mutex<Option<BTreeMap<String, u64>>> = Mutex::new(None);
/// Inbound names already warned about (warn once per process, then debug).
static WARNED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn with_map<F, R>(slot: &'static Mutex<Option<BTreeMap<String, String>>>, f: F) -> R
where
    F: FnOnce(&mut BTreeMap<String, String>) -> R,
{
    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(BTreeMap::new);
    f(map)
}

fn with_counter_map<F, R>(f: F) -> R
where
    F: FnOnce(&mut BTreeMap<String, u64>) -> R,
{
    let mut guard = FALLBACK_MODELS.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(BTreeMap::new);
    f(map)
}

fn with_warned<F, R>(f: F) -> R
where
    F: FnOnce(&mut HashSet<String>) -> R,
{
    let mut guard = WARNED.lock().unwrap_or_else(|e| e.into_inner());
    let set = guard.get_or_insert_with(HashSet::new);
    f(set)
}

/// Strip the `[1m]` context-window marker Claude Code appends to model ids
/// (`claude-opus-4-7[1m]`). Case-insensitive; repeated markers are stripped.
fn strip_context_marker(model: &str) -> &str {
    let mut s = model.trim();
    while s.len() >= 4 && s.to_ascii_lowercase().ends_with("[1m]") {
        s = s[..s.len() - 4].trim_end();
    }
    s
}

fn date_suffix() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Anthropic/Bedrock dated ids: claude-3-5-sonnet-20241022.
        regex::Regex::new(r"-(19|20)\d{6}$").expect("date-suffix regex is valid")
    })
}

fn bedrock_suffix() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Bedrock version marker: ...-v1:0
        regex::Regex::new(r"-v\d+:\d+$").expect("bedrock-suffix regex is valid")
    })
}

/// Progressively strip structural suffixes, returning every distinct candidate
/// in stripping order. `foo-20250101-latest` yields `foo-20250101` then `foo`.
fn structural_candidates(canonical: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = canonical.to_string();
    loop {
        let next = current
            .strip_suffix("-latest")
            .map(str::to_string)
            .or_else(|| {
                date_suffix()
                    .find(&current)
                    .map(|m| current[..m.start()].to_string())
            })
            .or_else(|| {
                bedrock_suffix()
                    .find(&current)
                    .map(|m| current[..m.start()].to_string())
            });
        match next {
            Some(candidate) if !candidate.is_empty() && candidate != current => {
                if !out.contains(&candidate) {
                    out.push(candidate.clone());
                }
                current = candidate;
            }
            _ => break,
        }
    }
    out
}

fn warn_once(inbound: &str) -> bool {
    with_warned(|set| {
        if set.len() >= MAX_TRACKED_NAMES {
            return false;
        }
        set.insert(inbound.to_string())
    })
}

fn record(resolution: &ModelResolution, inbound: &str) {
    match resolution.kind {
        ResolutionKind::Profile => {
            PROFILE_HITS.fetch_add(1, Ordering::Relaxed);
            debug!(
                inbound = inbound,
                upstream = resolution.upstream.as_str(),
                kind = resolution.kind.as_str(),
                "model resolved"
            );
        }
        ResolutionKind::Mapping => {
            MAPPING_HITS.fetch_add(1, Ordering::Relaxed);
            debug!(
                inbound = inbound,
                upstream = resolution.upstream.as_str(),
                kind = resolution.kind.as_str(),
                "model resolved"
            );
        }
        ResolutionKind::Normalized => {
            NORMALIZED_HITS.fetch_add(1, Ordering::Relaxed);
            if let Some(alias) = resolution.matched_alias.as_deref() {
                with_map(&NORMALIZED_MODELS, |map| {
                    if map.len() < MAX_TRACKED_NAMES || map.contains_key(inbound) {
                        map.insert(inbound.to_string(), alias.to_string());
                    }
                });
            }
            if warn_once(inbound) {
                warn!(
                    inbound = inbound,
                    matched = resolution.matched_alias.as_deref().unwrap_or(""),
                    upstream = resolution.upstream.as_str(),
                    "inbound model name needed suffix normalization; add an explicit mapping key to silence this"
                );
            } else {
                debug!(
                    inbound = inbound,
                    upstream = resolution.upstream.as_str(),
                    kind = resolution.kind.as_str(),
                    "model resolved"
                );
            }
        }
        ResolutionKind::Fallback => {
            FALLBACK_HITS.fetch_add(1, Ordering::Relaxed);
            with_counter_map(|map| {
                if map.len() < MAX_TRACKED_NAMES || map.contains_key(inbound) {
                    *map.entry(inbound.to_string()).or_insert(0) += 1;
                }
            });
            if warn_once(inbound) {
                warn!(
                    inbound = inbound,
                    upstream = resolution.upstream.as_str(),
                    "unknown model fell back to [models].default; add a mapping key if this is unintentional"
                );
            } else {
                debug!(
                    inbound = inbound,
                    upstream = resolution.upstream.as_str(),
                    kind = resolution.kind.as_str(),
                    "model resolved (fallback)"
                );
            }
        }
    }
}

/// Resolve an inbound (Anthropic-shaped) model name to the upstream model name.
pub fn resolve_model(inbound: &str, config: &Config) -> ModelResolution {
    let canonical = strip_context_marker(inbound);

    // Pass-through keeps the inbound spelling (including profile aliases).
    if config.model_profile(canonical).is_some() {
        let resolution = ModelResolution::new(canonical.to_string(), ResolutionKind::Profile, None);
        record(&resolution, inbound);
        return resolution;
    }

    if let Some(upstream) = config.model_mapping.get(canonical) {
        let resolution = ModelResolution::new(upstream.clone(), ResolutionKind::Mapping, None);
        record(&resolution, inbound);
        return resolution;
    }

    for candidate in structural_candidates(canonical) {
        if config.model_profile(&candidate).is_some() {
            let resolution = ModelResolution::new(
                candidate.clone(),
                ResolutionKind::Normalized,
                Some(candidate),
            );
            record(&resolution, inbound);
            return resolution;
        }
        if let Some(upstream) = config.model_mapping.get(&candidate) {
            let resolution = ModelResolution::new(
                upstream.clone(),
                ResolutionKind::Normalized,
                Some(candidate),
            );
            record(&resolution, inbound);
            return resolution;
        }
    }

    let resolution =
        ModelResolution::new(config.default_model.clone(), ResolutionKind::Fallback, None);
    record(&resolution, inbound);
    resolution
}

/// Snapshot of resolution counters for `/health`.
pub fn snapshot() -> Value {
    let normalized = with_map(&NORMALIZED_MODELS, |map| map.clone());
    let fallbacks = with_counter_map(|map| map.clone());
    json!({
        "profile_hits": PROFILE_HITS.load(Ordering::Relaxed),
        "mapping_hits": MAPPING_HITS.load(Ordering::Relaxed),
        "normalized_hits": NORMALIZED_HITS.load(Ordering::Relaxed),
        "fallbacks": FALLBACK_HITS.load(Ordering::Relaxed),
        "normalized_models": normalized,
        "fallback_models": fallbacks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_config;

    #[test]
    fn context_marker_is_stripped_for_mapping_hit() {
        let config = test_config();
        let resolution = resolve_model("claude-opus-4[1m]", &config);
        assert_eq!(resolution.kind, ResolutionKind::Mapping);
        assert_eq!(resolution.upstream, "deepseek-v4-pro");
    }

    #[test]
    fn profile_name_passes_through_verbatim() {
        let config = test_config();
        let resolution = resolve_model("deepseek-v4-flash", &config);
        assert_eq!(resolution.kind, ResolutionKind::Profile);
        assert_eq!(resolution.upstream, "deepseek-v4-flash");
    }

    #[test]
    fn dated_model_id_normalizes_to_base_mapping() {
        let config = test_config();
        let resolution = resolve_model("claude-sonnet-4-20250929", &config);
        assert_eq!(resolution.kind, ResolutionKind::Normalized);
        assert_eq!(resolution.upstream, "deepseek-v4-flash");
        assert_eq!(resolution.matched_alias.as_deref(), Some("claude-sonnet-4"));
    }

    #[test]
    fn latest_suffix_normalizes() {
        let config = test_config();
        let resolution = resolve_model("claude-opus-4-latest", &config);
        assert_eq!(resolution.kind, ResolutionKind::Normalized);
        assert_eq!(resolution.upstream, "deepseek-v4-pro");
    }

    #[test]
    fn bedrock_version_suffix_normalizes() {
        let config = test_config();
        let resolution = resolve_model("claude-sonnet-4-v1:0", &config);
        assert_eq!(resolution.kind, ResolutionKind::Normalized);
        assert_eq!(resolution.upstream, "deepseek-v4-flash");
    }

    #[test]
    fn chained_suffixes_normalize() {
        let config = test_config();
        let resolution = resolve_model("claude-opus-4-20250101-latest[1m]", &config);
        assert_eq!(resolution.kind, ResolutionKind::Normalized);
        assert_eq!(resolution.upstream, "deepseek-v4-pro");
    }

    #[test]
    fn dated_profile_name_normalizes_to_profile() {
        let config = test_config();
        let resolution = resolve_model("deepseek-v4-pro-20250101", &config);
        assert_eq!(resolution.kind, ResolutionKind::Normalized);
        assert_eq!(resolution.upstream, "deepseek-v4-pro");
    }

    #[test]
    fn unknown_model_falls_back_to_default() {
        let config = test_config();
        let resolution = resolve_model("claude-unknown-9[1m]", &config);
        assert_eq!(resolution.kind, ResolutionKind::Fallback);
        assert_eq!(resolution.upstream, config.default_model);
        assert!(resolution.matched_alias.is_none());
    }

    #[test]
    fn blank_model_falls_back_to_default() {
        let config = test_config();
        let resolution = resolve_model("   ", &config);
        assert_eq!(resolution.kind, ResolutionKind::Fallback);
        assert_eq!(resolution.upstream, config.default_model);
    }

    #[test]
    fn counters_and_fallback_models_are_recorded() {
        let config = test_config();
        let before = FALLBACK_HITS.load(Ordering::Relaxed);
        let _ = resolve_model("claude-observability-probe-model", &config);
        let after = FALLBACK_HITS.load(Ordering::Relaxed);
        assert!(after > before, "fallback counter must increase");
        let snapshot = snapshot();
        assert_eq!(
            snapshot["fallback_models"]["claude-observability-probe-model"],
            serde_json::json!(1)
        );

        let normalized_before = NORMALIZED_HITS.load(Ordering::Relaxed);
        let _ = resolve_model("claude-sonnet-4-20250101", &config);
        assert!(NORMALIZED_HITS.load(Ordering::Relaxed) > normalized_before);
    }

    #[test]
    fn structural_candidates_stop_when_nothing_is_strippable() {
        assert!(structural_candidates("claude-3-5-sonnet-20241022")
            .contains(&"claude-3-5-sonnet".to_string()));
        assert!(structural_candidates("glm-5.3").is_empty());
    }
}
