use serde::Serialize;

/// Which upstream the Codex harness talks to. Every supported provider exposes
/// an OpenAI-compatible `/v1/responses` endpoint, the only wire API Codex speaks,
/// plus a `/models` catalog we fetch live.
/// Local OpenCodex proxy port (translates Codex Responses -> any provider).
pub const OPENCODEX_PORT: u16 = 10100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    OpenRouter,
    Vercel,
    /// OpenCodex proxy — routes to 40+ upstream providers (Ollama, Anthropic,
    /// Gemini, Groq, Together, …) by `provider/model` id.
    OpenCodex,
}

impl Provider {
    pub fn parse(s: &str) -> Self {
        match s {
            "vercel" | "vercel_gateway" | "ai_gateway" => Provider::Vercel,
            "opencodex" | "ocx" => Provider::OpenCodex,
            _ => Provider::OpenRouter,
        }
    }

    pub const ALL: [Provider; 3] = [Provider::OpenRouter, Provider::Vercel, Provider::OpenCodex];

    /// UI identity of the composer selection (persisted in settings, used to
    /// filter the model catalog). NOT the Codex `model_provider` — every provider
    /// now routes through the local OpenCodex proxy (see `codex_provider_id`).
    pub fn id(self) -> &'static str {
        match self {
            Provider::OpenRouter => "openrouter",
            Provider::Vercel => "vercel",
            Provider::OpenCodex => "opencodex",
        }
    }

    /// The Codex `model_provider` block used for a thread. All three composer
    /// providers go through the OpenCodex proxy — OpenRouter and Vercel are just
    /// filtered views over ocx's catalog — so the upstream is chosen by the
    /// `provider/model` id prefix, not by a distinct Codex provider.
    pub fn codex_provider_id(self) -> &'static str {
        "opencodex"
    }

    /// The OpenCodex upstream registry name this composer provider maps to, and
    /// the `provider/…` prefix its models carry in ocx's catalog. `None` for the
    /// OpenCodex selection itself (shows every configured upstream).
    pub fn ocx_upstream(self) -> Option<&'static str> {
        match self {
            Provider::OpenRouter => Some("openrouter"),
            Provider::Vercel => Some("vercel-ai-gateway"),
            Provider::OpenCodex => None,
        }
    }

    /// Everything now talks to the local OpenCodex proxy.
    pub fn base_url(self) -> &'static str {
        "http://127.0.0.1:10100/v1"
    }

    pub fn models_url(self) -> &'static str {
        "http://127.0.0.1:10100/v1/models"
    }

    pub fn env_key(self) -> &'static str {
        match self {
            Provider::OpenRouter => "OPENROUTER_API_KEY",
            Provider::Vercel => "AI_GATEWAY_API_KEY",
            Provider::OpenCodex => "OPENCODEX_API_AUTH_TOKEN",
        }
    }

    /// The catalog is served by the local proxy, so no key is needed to list.
    pub fn models_need_key(self) -> bool {
        false
    }

    /// All providers are served by the OpenCodex proxy Hacksor manages.
    pub fn is_local_proxy(self) -> bool {
        true
    }

    pub fn display(self) -> &'static str {
        match self {
            Provider::OpenRouter => "OpenRouter",
            Provider::Vercel => "Vercel AI Gateway",
            Provider::OpenCodex => "OpenCodex (any provider)",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub id: &'static str,
    pub display: &'static str,
}

pub fn provider_list() -> Vec<ProviderInfo> {
    Provider::ALL
        .iter()
        .map(|p| ProviderInfo {
            id: p.id(),
            display: p.display(),
        })
        .collect()
}

/// A model as surfaced to the UI, fetched live from a provider catalog.
#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    /// Provider slug, passed straight to the Codex harness as the model id.
    pub id: String,
    pub display_name: String,
    pub description: String,
    pub context_length: Option<u64>,
    pub supports_vision: bool,
    pub prompt_price: Option<String>,
    /// Heuristic Codex reasoning effort for this model.
    pub reasoning_effort: String,
}

/// Parse the JSON array from `ocx models live --json` into the subset that
/// belongs to `upstream` (e.g. `openrouter`, `vercel-ai-gateway`), or the native
/// OpenCodex routing models when `upstream` is `None`. This is the authoritative
/// live catalog — ocx does NOT expose provider models on `/v1/models`, only its
/// own native models, so per-provider selection must read this instead.
pub fn parse_live_models(json: &str, upstream: Option<&str>) -> Vec<ModelInfo> {
    let arr: Vec<serde_json::Value> = serde_json::from_str(json).unwrap_or_default();
    let mut models: Vec<ModelInfo> = arr
        .iter()
        .filter(|m| match upstream {
            // A provider view: only that upstream's models.
            Some(u) => m.get("provider").and_then(|p| p.as_str()) == Some(u),
            // OpenCodex view: its native routing models.
            None => m.get("native").and_then(|n| n.as_bool()).unwrap_or(false),
        })
        .filter_map(parse_live_model)
        .collect();
    models.sort_by(|a, b| a.id.to_lowercase().cmp(&b.id.to_lowercase()));
    models
}

fn parse_live_model(v: &serde_json::Value) -> Option<ModelInfo> {
    // `namespaced` is the id ocx routes on (e.g. `openrouter/~anthropic/…`);
    // `id` is the shorter per-provider slug used as the label.
    let id = v.get("namespaced").and_then(|x| x.as_str())?.to_string();
    let display_name = v
        .get("id")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&id)
        .to_string();
    let context_length = v
        .get("contextWindow")
        .or_else(|| v.get("context_window"))
        .and_then(|c| c.as_u64());
    let supports_vision = v
        .get("inputModalities")
        .and_then(|m| m.as_array())
        .map(|a| a.iter().any(|x| x.as_str() == Some("image")))
        .unwrap_or(false);
    let reasoning_effort = v
        .get("defaultReasoningEffort")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| infer_effort(&id).to_string());
    Some(ModelInfo {
        id,
        display_name,
        description: String::new(),
        context_length,
        supports_vision,
        prompt_price: None,
        reasoning_effort,
    })
}

/// Heuristic mapping of a model slug to a Codex reasoning effort. Lightweight
/// "flash/mini/fast/nano" variants get low effort; everything else defaults high.
pub fn infer_effort(slug: &str) -> &'static str {
    let s = slug.to_lowercase();
    if s.contains("flash") || s.contains("mini") || s.contains("nano") || s.contains("fast") || s.contains("lite") || s.contains("haiku") {
        "low"
    } else if s.contains("air") || s.contains("small") {
        "medium"
    } else {
        "high"
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::truncate;

    #[test]
    fn provider_parse_roundtrips_ids() {
        for p in Provider::ALL {
            assert_eq!(Provider::parse(p.id()), p, "parse({}) should roundtrip", p.id());
        }
        // Unknown strings fall back to OpenRouter (the default provider).
        assert_eq!(Provider::parse("nonsense"), Provider::OpenRouter);
    }

    #[test]
    fn all_providers_route_through_opencodex_proxy() {
        // Every composer provider now talks to the local OpenCodex proxy on the
        // pinned port and creates its Codex thread under the "opencodex" block;
        // the upstream is chosen by the model-id prefix.
        for p in Provider::ALL {
            assert!(p.is_local_proxy(), "{} should route via the proxy", p.id());
            assert!(p.base_url().contains(&OPENCODEX_PORT.to_string()));
            assert!(p.models_url().contains(&OPENCODEX_PORT.to_string()));
            assert_eq!(p.codex_provider_id(), "opencodex");
            assert!(!p.models_need_key());
        }
        // Upstream mapping / catalog prefixes.
        assert_eq!(Provider::OpenRouter.ocx_upstream(), Some("openrouter"));
        assert_eq!(Provider::Vercel.ocx_upstream(), Some("vercel-ai-gateway"));
        assert_eq!(Provider::OpenCodex.ocx_upstream(), None);
        assert_eq!(Provider::OpenRouter.env_key(), "OPENROUTER_API_KEY");
    }

    #[test]
    fn infer_effort_matches_model_families() {
        assert_eq!(infer_effort("openai/gpt-4o-mini"), "low");
        assert_eq!(infer_effort("google/gemini-2.5-flash"), "low");
        assert_eq!(infer_effort("anthropic/claude-3.5-haiku"), "low");
        assert_eq!(infer_effort("z-ai/glm-4.6-air"), "medium");
        assert_eq!(infer_effort("deepseek/deepseek-r1"), "high");
        assert_eq!(infer_effort("openai/gpt-5"), "high");
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("hello", 3), "hel");
        assert_eq!(truncate("hi", 10), "hi");
        // Multi-byte characters must not be split mid-codepoint.
        assert_eq!(truncate("héllo", 2), "hé");
    }
}
