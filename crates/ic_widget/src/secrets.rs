//! Secrets in the OS credential store.
//!
//! The widget generates the bearer token `ironclaw-reborn serve` will accept,
//! and must hand the same token to the gateway (as an environment variable) and
//! to itself (as an HTTP header) on every launch. Persisting it in a config file
//! would put a live credential on disk in plaintext; regenerating it every
//! launch would work, but it makes an already-running gateway from a previous
//! session unreachable.
//!
//! So it lives in the Windows Credential Manager, via `keyring`. Nothing in this
//! crate ever writes a secret to a file, a log line, or an error message.

use crate::error::{Error, Result};
use crate::providers::Provider;

/// The service name every credential is filed under, shown in the Windows
/// Credential Manager UI.
const SERVICE: &str = "IronClaw Desktop";

/// The gateway bearer token's entry name.
const GATEWAY_TOKEN: &str = "gateway-token";

/// 32 bytes of hex. `serve` requires ≥ 32 bytes when SSO is enabled and uses the
/// token as an HMAC key (`serve.rs:272`); we are not using SSO, but a token that
/// would be too short for it is a token that would break the day we did.
const TOKEN_HEX_CHARS: usize = 64;

/// The OS credential store, scoped to this application.
///
/// Cheap to construct; every method talks to the OS.
#[derive(Debug, Clone, Copy, Default)]
pub struct SecretStore;

impl SecretStore {
    /// Open the store.
    pub fn new() -> Self {
        Self
    }

    /// The bearer token for `ironclaw-reborn serve`, minted on first use.
    ///
    /// Subsequent launches read back the same value, so a gateway left running
    /// by a previous session is still reachable.
    pub fn gateway_token(&self) -> Result<String> {
        match self.read(GATEWAY_TOKEN)? {
            Some(token) if token.len() >= TOKEN_HEX_CHARS => Ok(token),
            // A short or absent token is replaced. A short one can only come
            // from an older build with a weaker generator.
            _ => {
                let token = generate_token();
                self.write(GATEWAY_TOKEN, &token)?;
                tracing::info!("minted a new gateway token in the OS credential store");
                Ok(token)
            }
        }
    }

    /// Forget the gateway token. The next launch mints a fresh one, which
    /// orphans any gateway still running from a previous session.
    pub fn clear_gateway_token(&self) -> Result<()> {
        self.delete(GATEWAY_TOKEN)
    }

    /// A provider's stored API key.
    ///
    /// **This is the only method that returns a provider secret, and nothing
    /// that answers a Tauri command may call it.** The key exists to be handed
    /// to the gateway as an environment variable at spawn time; the dashboard
    /// asks [`SecretStore::has_provider_key`] instead, so a key the user pasted
    /// can never be read back out through the webview.
    pub fn provider_key(&self, provider: &Provider) -> Result<Option<String>> {
        self.read(&provider.secret_entry())
    }

    /// Whether a key is stored for this provider. Safe to expose to the UI.
    pub fn has_provider_key(&self, provider: &Provider) -> Result<bool> {
        Ok(self.read(&provider.secret_entry())?.is_some())
    }

    /// Store a provider's API key, replacing any previous one.
    ///
    /// An empty or blank key is rejected rather than stored: it would look
    /// configured in the dashboard and then fail the gateway at boot with an
    /// authentication error that names the provider, not the empty key.
    pub fn set_provider_key(&self, provider: &Provider, key: &str) -> Result<()> {
        if key.trim().is_empty() {
            return Err(Error::BlankProviderKey {
                provider: provider.id.clone(),
            });
        }
        self.write(&provider.secret_entry(), key)?;
        // The provider id is safe to log. The key is not, and is not in scope
        // for this line.
        tracing::info!(provider = %provider.id, "stored a provider key in the OS credential store");
        Ok(())
    }

    /// Forget a provider's API key.
    pub fn clear_provider_key(&self, provider: &Provider) -> Result<()> {
        self.delete(&provider.secret_entry())?;
        tracing::info!(provider = %provider.id, "cleared a provider key from the OS credential store");
        Ok(())
    }

    /// Remove every credential this app owns: the gateway token and each provider's
    /// key. For uninstall cleanup — Windows never removes Credential Manager entries
    /// on its own. Idempotent (a missing entry is not an error), and best-effort per
    /// entry: one unreadable provider does not abort the rest.
    pub fn clear_all(&self) -> Result<()> {
        self.clear_gateway_token()?;
        // The credential store cannot be enumerated, so we delete the known keys:
        // one per provider in the catalog.
        let providers = crate::providers::all().unwrap_or_default();
        for provider in providers {
            if let Err(error) = self.clear_provider_key(&provider) {
                tracing::warn!(provider = %provider.id, %error, "could not clear a provider key");
            }
        }
        Ok(())
    }

    fn entry(&self, name: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(SERVICE, name).map_err(|source| Error::Keyring {
            operation: "open",
            entry: name.to_string(),
            source,
        })
    }

    fn read(&self, name: &str) -> Result<Option<String>> {
        match self.entry(name)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(source) => Err(Error::Keyring {
                operation: "read",
                entry: name.to_string(),
                source,
            }),
        }
    }

    fn write(&self, name: &str, secret: &str) -> Result<()> {
        self.entry(name)?
            .set_password(secret)
            .map_err(|source| Error::Keyring {
                operation: "store",
                entry: name.to_string(),
                source,
            })
    }

    fn delete(&self, name: &str) -> Result<()> {
        match self.entry(name)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(source) => Err(Error::Keyring {
                operation: "delete",
                entry: name.to_string(),
                source,
            }),
        }
    }
}

/// 128 bits of randomness rendered as 64 hex characters, from two v4 UUIDs.
///
/// `uuid`'s v4 generator draws from the OS CSPRNG, which is the same source a
/// dedicated `rand` dependency would use.
fn generate_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_token_is_long_enough_for_sso_hmac_use() {
        let token = generate_token();
        assert_eq!(token.len(), TOKEN_HEX_CHARS);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        // `serve` treats the token as an HMAC key when SSO is on and rejects
        // anything under 32 bytes.
        assert!(token.len() >= 32);
    }

    #[test]
    fn tokens_are_not_reused_between_generations() {
        assert_ne!(generate_token(), generate_token());
    }

    #[test]
    fn a_blank_provider_key_is_refused_before_it_reaches_the_credential_store() {
        let anthropic = crate::providers::find("anthropic")
            .expect("decode")
            .expect("exists");
        let store = SecretStore::new();

        // Whitespace counts as blank. A stored blank key looks configured in
        // the dashboard and then fails the gateway at boot.
        for blank in ["", "   ", "\t\n"] {
            let error = store
                .set_provider_key(&anthropic, blank)
                .expect_err("a blank key must be refused");
            assert!(matches!(error, Error::BlankProviderKey { .. }));
            // The message names the provider, never the value.
            assert!(error.to_string().contains("anthropic"));
        }
    }

    #[test]
    fn a_provider_key_entry_never_collides_with_the_gateway_token() {
        for provider in crate::providers::api_key_providers().expect("decode") {
            assert_ne!(
                provider.secret_entry(),
                GATEWAY_TOKEN,
                "a provider key would overwrite the gateway bearer token"
            );
        }
    }

    /// Exercises the real Credential Manager. Ignored by default: it writes to
    /// the developer's own credential store, and CI runners have no unlocked
    /// keyring on every platform.
    #[test]
    #[ignore = "touches the real OS credential store"]
    fn a_provider_key_round_trips_and_is_reported_as_present() {
        let anthropic = crate::providers::find("anthropic")
            .expect("decode")
            .expect("exists");
        let store = SecretStore::new();
        store.clear_provider_key(&anthropic).expect("start clean");

        assert!(!store.has_provider_key(&anthropic).expect("absent"));

        store
            .set_provider_key(&anthropic, "sk-ant-test")
            .expect("store");
        assert!(store.has_provider_key(&anthropic).expect("present"));
        assert_eq!(
            store.provider_key(&anthropic).expect("read").as_deref(),
            Some("sk-ant-test")
        );

        store.clear_provider_key(&anthropic).expect("clear");
        assert!(!store.has_provider_key(&anthropic).expect("absent again"));
    }

    /// Exercises the real Credential Manager. Ignored by default: it writes to
    /// the developer's own credential store, and CI runners have no unlocked
    /// keyring on every platform.
    #[test]
    #[ignore = "touches the real OS credential store"]
    fn the_gateway_token_round_trips_through_the_credential_store() {
        let store = SecretStore::new();
        store.clear_gateway_token().expect("start clean");

        let minted = store.gateway_token().expect("mint");
        let read_back = store.gateway_token().expect("read back");
        assert_eq!(
            minted, read_back,
            "a second launch must reuse the token, or it cannot reach a gateway \
             left running by the first"
        );

        store.clear_gateway_token().expect("clear");
        assert_ne!(
            store.gateway_token().expect("re-mint"),
            minted,
            "clearing must actually forget the token"
        );
        store.clear_gateway_token().expect("clean up");
    }
}
