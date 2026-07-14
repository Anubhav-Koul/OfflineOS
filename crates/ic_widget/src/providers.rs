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
    /// The environment variable this provider reads its endpoint from. Needed
    /// for the escape hatch: `openai_compatible` has no endpoint until the user
    /// gives it one (`LLM_BASE_URL`).
    #[serde(default)]
    pub base_url_env: Option<String>,
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
    /// Setup metadata, including where the user goes to get a key.
    #[serde(default)]
    pub setup: Option<Setup>,
}

/// The catalog's setup block — the part the directory needs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Setup {
    /// Where to sign up for a key. Shown as a link so the user is not left
    /// guessing which of a vendor's five consoles mints the right token.
    #[serde(default)]
    pub key_url: Option<String>,
    /// The vendor's own name for itself, nicer than the id.
    #[serde(default)]
    pub display_name: Option<String>,
}

impl Provider {
    /// Whether this provider is configured by pasting an API key.
    pub fn takes_api_key(&self) -> bool {
        self.api_key_env.is_some()
    }

    /// The endpoint to probe (and to fail over to). `None` means the user must
    /// supply one — the escape hatch for `openai_compatible` and anything
    /// self-hosted.
    ///
    /// **This must be the URL the gateway itself will use**, or a green tick in
    /// the panel would prove nothing about the model that actually runs. Most of
    /// the catalog declares `default_base_url`; the handful that do not are the
    /// vendors whose SDK carries a well-known default, which is what the table
    /// below restores.
    pub fn probe_base_url(&self) -> Option<String> {
        if let Some(url) = self
            .default_base_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
        {
            return Some(url.to_string());
        }
        // The well-known defaults the catalog omits because each vendor's client
        // hardcodes them.
        let known = match self.id.as_str() {
            "openai" => "https://api.openai.com/v1",
            "anthropic" => "https://api.anthropic.com/v1",
            "openrouter" => "https://openrouter.ai/api/v1",
            "deepseek" => "https://api.deepseek.com/v1",
            "gemini" => "https://generativelanguage.googleapis.com/v1beta",
            // `openai_compatible` and `cloudflare` are deliberately absent: their
            // endpoint is the user's own, and guessing one would be a lie.
            _ => return None,
        };
        Some(known.to_string())
    }

    /// Whether the widget knows how to ask this provider "does this key work?".
    ///
    /// The out-of-band authenticators (Bedrock's AWS chain, Codex's device flow,
    /// NEAR AI's session, Copilot's token exchange) are not probeable with a
    /// pasted key, and Ollama takes no key at all. Saying so is better than a
    /// green tick that means nothing — which is exactly the trap the gateway's own
    /// probe falls into.
    pub fn is_probeable(&self) -> bool {
        self.takes_api_key()
            && matches!(
                self.protocol.as_deref(),
                Some("open_ai_completions" | "open_router" | "anthropic" | "gemini" | "deep_seek")
            )
    }

    /// Where the user gets a key.
    pub fn key_url(&self) -> Option<&str> {
        self.setup.as_ref()?.key_url.as_deref()
    }

    /// The vendor's own name for itself, falling back to the id.
    pub fn display_name(&self) -> &str {
        self.setup
            .as_ref()
            .and_then(|setup| setup.display_name.as_deref())
            .unwrap_or(&self.id)
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
        // Anthropic's native protocol is its own, but it publishes an
        // OpenAI-compatible layer on the same origin — the one deliberate
        // exception. OpenRouter's HTTP surface is OpenAI-shaped too.
        let openai_shaped = matches!(
            self.protocol.as_deref(),
            Some("open_ai_completions" | "open_router")
        ) || self.id == "anthropic";
        openai_shaped.then(|| self.probe_base_url()).flatten()
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
    /// `base_url` overrides the endpoint — required for `openai_compatible`,
    /// which has none until the user supplies one, and honoured for any provider
    /// the user points somewhere else (a proxy, a regional endpoint).
    pub fn llm_env(
        &self,
        api_key: &str,
        model: Option<&str>,
        base_url: Option<&str>,
    ) -> Option<Vec<(String, String)>> {
        let key_env = self.api_key_env.as_ref()?;
        let mut env = vec![
            ("LLM_BACKEND".to_string(), self.id.clone()),
            (key_env.clone(), api_key.to_string()),
            (
                "LLM_MODEL".to_string(),
                model.unwrap_or(&self.default_model).to_string(),
            ),
        ];
        // The gateway reads the endpoint from the provider's *own* variable, the
        // same way it reads the key from the provider's own key variable.
        if let (Some(url_env), Some(url)) = (
            self.base_url_env.as_ref(),
            base_url.map(str::trim).filter(|url| !url.is_empty()),
        ) {
            env.push((url_env.clone(), url.to_string()));
        }
        Some(env)
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
            .llm_env("sk-test", None, None)
            .expect("anthropic takes a key")
            .into_iter()
            .collect();
        assert_eq!(env["LLM_BACKEND"], "anthropic");
        assert_eq!(env["ANTHROPIC_API_KEY"], "sk-test");
        assert_eq!(env["LLM_MODEL"], anthropic.default_model);

        // An explicit model wins over the catalog default.
        let env: std::collections::HashMap<_, _> = anthropic
            .llm_env("sk-test", Some("claude-opus-4-8"), None)
            .expect("anthropic takes a key")
            .into_iter()
            .collect();
        assert_eq!(env["LLM_MODEL"], "claude-opus-4-8");
    }

    #[test]
    fn a_provider_without_a_key_variable_yields_no_environment() {
        let ollama = find("ollama").expect("decode").expect("ollama must exist");
        assert!(!ollama.takes_api_key());
        assert_eq!(ollama.llm_env("ignored", None, None), None);
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

    /// Every provider we offer to test must have somewhere to send the test —
    /// except the ones whose endpoint is *by nature* the user's own, which the
    /// panel prompts for. Pinning the set means a new catalog entry with no URL
    /// fails here rather than silently probing nowhere.
    #[test]
    fn the_only_probeable_providers_without_an_endpoint_are_the_bring_your_own_ones() {
        let endpointless: Vec<String> = api_key_providers()
            .expect("decode")
            .into_iter()
            .filter(|provider| provider.is_probeable() && provider.probe_base_url().is_none())
            .map(|provider| provider.id)
            .collect();

        // `openai_compatible` is whatever the user is running; `cloudflare`'s URL
        // embeds their account id. Neither can be guessed, and the panel asks.
        assert_eq!(
            endpointless,
            vec!["openai_compatible".to_string(), "cloudflare".to_string()],
            "a probeable provider with no endpoint would send the probe nowhere — \
             either give it a URL or make the panel ask for one"
        );
    }

    #[test]
    fn the_bulk_of_the_catalog_is_probeable_and_the_out_of_band_ones_are_not() {
        let probeable: Vec<String> = api_key_providers()
            .expect("decode")
            .into_iter()
            .filter(Provider::is_probeable)
            .map(|provider| provider.id)
            .collect();

        // The ones a user is most likely to reach for.
        for id in [
            "openai",
            "anthropic",
            "openrouter",
            "groq",
            "mistral",
            "gemini",
        ] {
            assert!(
                probeable.contains(&id.to_string()),
                "{id} should be probeable"
            );
        }
        // And the ones that authenticate out of band, which a pasted key cannot
        // test — saying so beats a green tick that means nothing.
        let copilot = find("github_copilot").expect("decode").expect("exists");
        assert!(
            !copilot.is_probeable(),
            "Copilot exchanges its token for a session; a pasted token cannot be \
             checked with a plain listing call"
        );
    }

    #[test]
    fn openrouter_is_reachable_because_one_key_there_covers_most_models() {
        let openrouter = find("openrouter").expect("decode").expect("exists");
        assert_eq!(
            openrouter.probe_base_url().as_deref(),
            Some("https://openrouter.ai/api/v1")
        );
        assert!(openrouter.is_probeable());
        assert!(openrouter.can_fail_over(), "and it can serve as a fallback");
        assert!(
            openrouter.key_url().is_some(),
            "and we can link the signup page"
        );
    }

    #[test]
    fn the_escape_hatch_has_no_guessed_endpoint() {
        // `openai_compatible` is whatever the user is running. Inventing a URL
        // for it would be a lie the probe would then "verify".
        let compatible = find("openai_compatible").expect("decode").expect("exists");
        assert_eq!(compatible.probe_base_url(), None);
        assert!(
            compatible.is_probeable(),
            "but it can be probed once given one"
        );
    }

    #[test]
    fn secret_entries_are_namespaced_away_from_the_gateway_token() {
        let anthropic = find("anthropic").expect("decode").expect("exists");
        assert_eq!(anthropic.secret_entry(), "provider-key/anthropic");
        assert_ne!(anthropic.secret_entry(), "gateway-token");
    }
}
