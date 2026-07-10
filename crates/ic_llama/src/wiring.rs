//! The IronClaw integration surface — four environment variables.
//!
//! IronClaw ships an `openai_compatible` LLM provider that routes through
//! `RigAdapter` and speaks the OpenAI Chat Completions API. `llama-server`
//! serves exactly that API. So running a local model needs **no changes to any
//! IronClaw core crate**: point `LLM_BASE_URL` at the sidecar and the agent loop
//! is talking to llama.cpp.
//!
//! Setting `LLM_BACKEND` also stops IronClaw from consulting any other
//! provider's environment (a developer's `ANTHROPIC_API_KEY`, the NEAR AI
//! default), so a run that is supposed to be local is local.
//!
//! ```no_run
//! # use ic_llama::wiring::LlmEnv;
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let sidecar: ic_llama::server::Sidecar = unimplemented!();
//! let mut command = std::process::Command::new("ironclaw-reborn");
//! LlmEnv::for_sidecar(&sidecar).apply(&mut command);
//! command.arg("serve").spawn()?;
//! # Ok(())
//! # }
//! ```

use std::collections::BTreeMap;
use std::fmt;

use crate::server::Sidecar;

/// Selects IronClaw's generic OpenAI-compatible provider.
pub const LLM_BACKEND: &str = "LLM_BACKEND";
/// The provider's base URL, including the `/v1` suffix.
pub const LLM_BASE_URL: &str = "LLM_BASE_URL";
/// The bearer token sent to the provider.
pub const LLM_API_KEY: &str = "LLM_API_KEY";
/// The model name sent in each request.
pub const LLM_MODEL: &str = "LLM_MODEL";

/// The value `LLM_BACKEND` must take.
pub const OPENAI_COMPATIBLE: &str = "openai_compatible";

/// The environment that points IronClaw at a local `llama-server`.
///
/// Its [`fmt::Debug`] redacts the API key: this struct ends up in the widget's
/// process-supervision logs, and a key that leaks into a log file the user
/// pastes into a bug report is a key that leaks.
#[derive(Clone, PartialEq, Eq)]
pub struct LlmEnv {
    vars: BTreeMap<&'static str, String>,
}

impl LlmEnv {
    /// Build the environment for an arbitrary OpenAI-compatible endpoint.
    pub fn openai_compatible(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            vars: BTreeMap::from([
                (LLM_BACKEND, OPENAI_COMPATIBLE.to_string()),
                (LLM_BASE_URL, base_url.into()),
                (LLM_API_KEY, api_key.into()),
                (LLM_MODEL, model.into()),
            ]),
        }
    }

    /// Build the environment for a running sidecar.
    pub fn for_sidecar(sidecar: &Sidecar) -> Self {
        Self::openai_compatible(
            sidecar.base_url(),
            sidecar.api_key(),
            sidecar.model_id().as_str(),
        )
    }

    /// The variables, in a stable order.
    pub fn vars(&self) -> impl Iterator<Item = (&'static str, &str)> {
        self.vars
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
    }

    /// Look one up.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(String::as_str)
    }

    /// Apply to a child process about to be spawned.
    pub fn apply(&self, command: &mut std::process::Command) {
        for (name, value) in self.vars() {
            command.env(name, value);
        }
    }

    /// Apply to a tokio child process about to be spawned.
    pub fn apply_tokio(&self, command: &mut tokio::process::Command) {
        for (name, value) in self.vars() {
            command.env(name, value);
        }
    }
}

impl fmt::Debug for LlmEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("LlmEnv");
        for (name, value) in self.vars() {
            if name == LLM_API_KEY {
                debug.field(name, &"<redacted>");
            } else {
                debug.field(name, &value);
            }
        }
        debug.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> LlmEnv {
        LlmEnv::openai_compatible("http://127.0.0.1:8080/v1", "secret-key", "Qwen3-4B-Q4_K_M")
    }

    #[test]
    fn the_backend_is_pinned_to_the_openai_compatible_provider() {
        assert_eq!(env().get(LLM_BACKEND), Some(OPENAI_COMPATIBLE));
    }

    #[test]
    fn the_base_url_keeps_the_v1_suffix_the_provider_expects() {
        assert_eq!(env().get(LLM_BASE_URL), Some("http://127.0.0.1:8080/v1"));
    }

    #[test]
    fn exactly_the_four_provider_variables_are_set() {
        let names: Vec<_> = env().vars().map(|(name, _)| name).collect();
        assert_eq!(names, [LLM_API_KEY, LLM_BACKEND, LLM_BASE_URL, LLM_MODEL]);
    }

    #[test]
    fn the_api_key_never_appears_in_debug_output() {
        let rendered = format!("{:?}", env());
        assert!(!rendered.contains("secret-key"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        // The other values are still there, which is the point of logging it.
        assert!(rendered.contains("Qwen3-4B-Q4_K_M"), "{rendered}");
    }

    #[test]
    fn applying_sets_every_variable_on_the_command() {
        let mut command = std::process::Command::new("ironclaw-reborn");
        env().apply(&mut command);
        let applied: Vec<_> = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(applied.contains(&(LLM_BACKEND.to_string(), Some(OPENAI_COMPATIBLE.to_string()))));
        assert_eq!(applied.len(), 4);
    }
}
