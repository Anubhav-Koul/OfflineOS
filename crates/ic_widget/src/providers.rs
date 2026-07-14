//! The LLM provider catalog, read from the file the gateway itself resolves
//! `LLM_BACKEND` against.
//!
//! `ironclaw-reborn` does not read a per-provider set of environment variables
//! we get to invent. It matches `LLM_BACKEND` against the ids and aliases in
//! `providers.json`, then reads *that provider's own* key variable —
//! `ANTHROPIC_API_KEY`, never a generic `LLM_API_KEY`. See
//! `ironclaw_llm::resolution::resolve_provider_config_from_env`.
//!
//! So the dashboard cannot hardcode a provider list: the catalog is
//! user-extensible, and a provider added upstream must appear without a code
//! change here. This module embeds the same JSON `ironclaw_llm::registry`
//! compiles in, so the two cannot disagree about what `anthropic` means.
//!
//! Only the fields the dashboard needs are decoded. Unknown fields are ignored
//! rather than rejected, so an upstream addition to the schema does not break
//! this build.

use serde::Deserialize;

use crate::error::{Error, Result};

/// `providers.json`, embedded at compile time — the same copy
/// `ironclaw_llm::registry::builtin_provider_definitions` compiles in.
const CATALOG_JSON: &str = include_str!("../../../providers.json");

/// One entry in the provider catalog.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Provider {
    /// The value that selects this provider in `LLM_BACKEND`.
    pub id: String,
    /// The environment variable this provider reads its API key from.
    ///
    /// `None` for providers that need no key (`ollama`) or that authenticate
    /// out of band (`gemini_oauth`, `openai_codex`). Those cannot be configured
    /// by pasting a key, so [`api_key_providers`] filters them out.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// The wire dialect this provider speaks: `open_ai_completions`,
    /// `anthropic`, `deep_seek`, … Only OpenAI-shaped endpoints can serve as a
    /// [failover target](Provider::failover_base_url), because the proxy
    /// forwards the request in the shape the gateway already produced.
    #[serde(default)]
    pub protocol: Option<String>,
    /// The endpoint the provider is reached at when it has a fixed one.
    /// Absent (or empty) for providers whose URL is a well-known default the
    /// SDK carries, and for `openai_compatible`, whose URL is the user's.
    #[serde(default)]
    pub default_base_url: Option<String>,
    /// The model used when the user does not choose one.
    pub default_model: String,
    /// A one-line description, shown beside the key field.
    pub description: String,
}

impl Provider {
    /// Whether this provider is configured by pasting an API key.
    pub fn takes_api_key(&self) -> bool {
        self.api_key_env.is_some()
    }

    /// The OpenAI-shaped chat-completions base URL to fail over to, or `None`
    /// when this provider cannot serve as a fallback at all.
    ///
    /// The proxy forwards the request body the gateway already built, so a
    /// fallback must speak the OpenAI Chat Completions dialect. That rules out
    /// providers whose `protocol` is something else — with one deliberate
    /// exception: **Anthropic**, whose native protocol is its own, publishes an
    /// OpenAI-compatible layer on the same origin. That is the documented cost
    /// of the route-around (`docs/desktop/llm-provider-selection.md`): a
    /// fallback reaches a provider's compatible surface, not its native one.
    pub fn failover_base_url(&self) -> Option<String> {
        // The two the fork names in its own definition of done. Their endpoints
        // are stable and well known; the catalog carries no URL for either
        // because their SDKs default to it.
        match self.id.as_str() {
            "anthropic" => return Some("https://api.anthropic.com/v1".to_string()),
            "openai" => return Some("https://api.openai.com/v1".to_string()),
            _ => {}
        }
        if self.protocol.as_deref() != Some("open_ai_completions") {
            return None;
        }
        self.default_base_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string)
    }

    /// Whether this provider can be the local model's cloud fallback: it takes
    /// a pasted key, and it can be reached in an OpenAI-shaped way.
    pub fn can_fail_over(&self) -> bool {
        self.takes_api_key() && self.failover_base_url().is_some()
    }

    /// This provider's entry name in the OS credential store.
    ///
    /// Namespaced so it cannot collide with `gateway-token`, and stable across
    /// releases: renaming it would strand the user's stored key.
    pub fn secret_entry(&self) -> String {
        format!("provider-key/{}", self.id)
    }

    /// The environment this provider needs in order to be the active backend.
    ///
    /// `model` overrides [`Provider::default_model`] when the user picked one.
    /// Returns `None` for a provider that takes no key, because there is
    /// nothing for the dashboard to inject.
    pub fn llm_env(&self, api_key: &str, model: Option<&str>) -> Option<Vec<(String, String)>> {
        let key_env = self.api_key_env.as_ref()?;
        Some(vec![
            ("LLM_BACKEND".to_string(), self.id.clone()),
            (key_env.clone(), api_key.to_string()),
            (
                "LLM_MODEL".to_string(),
                model.unwrap_or(&self.default_model).to_string(),
            ),
        ])
    }
}

/// Every provider in the catalog, in file order.
pub fn all() -> Result<Vec<Provider>> {
    serde_json::from_str(CATALOG_JSON).map_err(|source| Error::Json {
        context: "the embedded providers.json could not be decoded".to_string(),
        source,
    })
}

/// The providers the dashboard can configure: those that read a key from the
/// environment. Ordered as they appear in the catalog.
pub fn api_key_providers() -> Result<Vec<Provider>> {
    Ok(all()?.into_iter().filter(Provider::takes_api_key).collect())
}

/// The providers that can serve as the local model's cloud fallback: they take
/// a pasted key *and* can be reached in an OpenAI-shaped way. See
/// [`Provider::failover_base_url`] for why that is a real restriction.
pub fn failover_providers() -> Result<Vec<Provider>> {
    Ok(all()?.into_iter().filter(Provider::can_fail_over).collect())
}

/// Look a provider up by its `LLM_BACKEND` id. Aliases are not resolved here —
/// the dashboard only ever hands back an id it read from [`all`].
pub fn find(id: &str) -> Result<Option<Provider>> {
    Ok(all()?.into_iter().find(|provider| provider.id == id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_catalog_decodes_and_carries_the_providers_we_name_in_the_ui() {
        let providers = all().expect("the embedded catalog must decode");
        assert!(
            providers.len() > 5,
            "a catalog this small means the file moved or the schema changed"
        );

        let anthropic = find("anthropic")
            .expect("decode")
            .expect("anthropic must exist");
        // The key variable is the provider's own, not a generic one. Getting
        // this wrong means the gateway silently starts with no credentials.
        assert_eq!(anthropic.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));

        let openai = find("openai").expect("decode").expect("openai must exist");
        assert_eq!(openai.api_key_env.as_deref(), Some("OPENAI_API_KEY"));
    }

    #[test]
    fn providers_that_authenticate_out_of_band_are_not_offered_a_key_field() {
        let configurable = api_key_providers().expect("decode");
        let ids: Vec<&str> = configurable.iter().map(|p| p.id.as_str()).collect();

        assert!(ids.contains(&"anthropic"));
        assert!(ids.contains(&"openai"));
        // OAuth and subscription providers have no `api_key_env`. Showing a key
        // field for them would invite the user to paste a secret that nothing
        // reads.
        assert!(!ids.contains(&"gemini_oauth"));
        assert!(!ids.contains(&"openai_codex"));
        assert!(!ids.contains(&"ollama"));
    }

    #[test]
    fn the_environment_names_the_backend_the_key_and_the_model() {
        let anthropic = find("anthropic").expect("decode").expect("exists");

        let env: std::collections::HashMap<_, _> = anthropic
            .llm_env("sk-test", None)
            .expect("anthropic takes a key")
            .into_iter()
            .collect();
        assert_eq!(env["LLM_BACKEND"], "anthropic");
        assert_eq!(env["ANTHROPIC_API_KEY"], "sk-test");
        assert_eq!(env["LLM_MODEL"], anthropic.default_model);

        // An explicit model wins over the catalog default.
        let env: std::collections::HashMap<_, _> = anthropic
            .llm_env("sk-test", Some("claude-opus-4-8"))
            .expect("anthropic takes a key")
            .into_iter()
            .collect();
        assert_eq!(env["LLM_MODEL"], "claude-opus-4-8");
    }

    #[test]
    fn a_provider_without_a_key_variable_yields_no_environment() {
        let ollama = find("ollama").expect("decode").expect("ollama must exist");
        assert!(!ollama.takes_api_key());
        assert_eq!(ollama.llm_env("ignored", None), None);
    }

    #[test]
    fn the_failover_list_holds_the_two_providers_v1_promises_and_reaches_them_openai_shaped() {
        let ids: Vec<String> = failover_providers()
            .expect("decode")
            .into_iter()
            .map(|provider| provider.id)
            .collect();
        // The definition of done says "cloud failover when a key is configured",
        // and these are the two it means.
        assert!(ids.contains(&"anthropic".to_string()));
        assert!(ids.contains(&"openai".to_string()));

        let anthropic = find("anthropic").expect("decode").expect("exists");
        assert_eq!(
            anthropic.failover_base_url().as_deref(),
            Some("https://api.anthropic.com/v1"),
            "Anthropic's native protocol is not OpenAI-shaped; failover reaches \
             its compatible layer, which is the documented cost of the route-around"
        );
    }

    #[test]
    fn a_provider_that_speaks_another_dialect_is_not_offered_as_a_fallback() {
        // The proxy forwards the body the gateway already built. A provider that
        // cannot read that shape would fail every failover — offering it would
        // be a promise the fork cannot keep.
        let deepseek = find("deepseek").expect("decode").expect("exists");
        assert_eq!(deepseek.protocol.as_deref(), Some("deep_seek"));
        assert!(!deepseek.can_fail_over());

        // And one that needs no key cannot be a *cloud* fallback either.
        let ollama = find("ollama").expect("decode").expect("exists");
        assert!(!ollama.can_fail_over());
    }

    #[test]
    fn an_openai_dialect_provider_with_a_catalog_url_is_offered() {
        let groq = find("groq").expect("decode").expect("exists");
        assert_eq!(groq.protocol.as_deref(), Some("open_ai_completions"));
        assert_eq!(
            groq.failover_base_url().as_deref(),
            Some("https://api.groq.com/openai/v1"),
            "a catalog-declared endpoint is used as it stands"
        );
    }

    #[test]
    fn secret_entries_are_namespaced_away_from_the_gateway_token() {
        let anthropic = find("anthropic").expect("decode").expect("exists");
        assert_eq!(anthropic.secret_entry(), "provider-key/anthropic");
        assert_ne!(anthropic.secret_entry(), "gateway-token");
    }
}
