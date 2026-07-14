//! Does this key actually work? (Phase 8a.5)
//!
//! The gateway cannot answer that question. `POST /llm/test-connection` reports
//! `ok: true` for a dead endpoint with a junk key — `RigAdapter` never implements
//! `list_models`, so the probe never opens a socket — and `POST /llm/providers`
//! answers `503` for every protocol, because the operator LLM-config service is
//! not composed under our profile. Both are pinned by integration tests
//! (`chat_control.rs`, `provider_protocols.rs`) which will start failing the day
//! upstream fixes either, and that is the signal to delete this module.
//!
//! So the widget asks the provider itself. It already holds the key (Windows
//! Credential Manager) and knows the endpoint, so this is a direct, honest
//! question with no runtime in the middle.
//!
//! # Probing by protocol family, not by brand
//!
//! There are 26 providers and 11 protocols in `providers.json`, but only a
//! handful of *shapes*. We probe the shape:
//!
//! - **OpenAI-compatible** (16 of the 26, plus OpenRouter): `GET {base}/models`
//!   with a bearer token. The catch: some compatible servers do not enforce auth
//!   on that route, so a `200` there proves the endpoint exists — not that the
//!   key is good. When we detect that (a deliberately bogus key also gets a
//!   `200`), we fall back to a **one-token completion**, which is the only probe
//!   that truly validates a key.
//! - **Anthropic**: `GET /v1/models` with `x-api-key` + `anthropic-version`.
//! - **Gemini**: `GET /v1beta/models?key=…` (the key is a query parameter).
//! - Everything else (Bedrock, Codex, NEAR AI, Copilot's token exchange, Ollama):
//!   not probed. They authenticate out of band, and pretending otherwise would be
//!   the same lie the gateway's own probe tells.

use std::time::Duration;

use serde::Serialize;

use crate::providers::Provider;

/// A probe waits this long. A provider that cannot answer a model list in ten
/// seconds is not one a user should be waiting on behind a button.
const TIMEOUT: Duration = Duration::from_secs(10);

/// What a probe found, in terms a person can act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Probe {
    /// The key works. `models` may be empty when the provider exposes no list —
    /// the key is still good.
    Ok {
        /// What the provider says it can run, if it says.
        models: Vec<String>,
        /// How we know, in one line.
        message: String,
    },
    /// The endpoint answered, and refused the key.
    KeyRejected {
        /// What the provider said, trimmed.
        message: String,
    },
    /// The endpoint could not be reached at all — wrong URL, no network, DNS.
    Unreachable {
        /// The transport failure, in words.
        message: String,
    },
    /// The key is fine; the provider is throttling us.
    RateLimited {
        /// What the provider said.
        message: String,
    },
    /// We do not know how to ask this provider. Better to say so than to guess.
    Unsupported {
        /// Why.
        message: String,
    },
}

impl Probe {
    /// Whether the panel should show a green tick.
    pub fn is_ok(&self) -> bool {
        matches!(self, Probe::Ok { .. })
    }
}

/// Ask `provider` whether `api_key` works, and what it can run.
///
/// `base_url_override` is for the escape hatch: `openai_compatible` and any
/// self-hosted endpoint, where the user supplies the URL.
pub async fn probe(provider: &Provider, api_key: &str, base_url_override: Option<&str>) -> Probe {
    let client = match reqwest::Client::builder().timeout(TIMEOUT).build() {
        Ok(client) => client,
        Err(error) => {
            return Probe::Unreachable {
                message: format!("could not build an HTTP client: {error}"),
            };
        }
    };

    let base = match base_url_override
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_string)
        .or_else(|| provider.probe_base_url())
    {
        Some(base) => base.trim_end_matches('/').to_string(),
        None => {
            return Probe::Unsupported {
                message: "this provider needs a base URL — enter one below".to_string(),
            };
        }
    };

    match provider.protocol.as_deref() {
        // The bulk of the catalog. OpenRouter speaks this shape too, even though
        // the runtime gives it its own adapter for reasoning round-tripping.
        Some("open_ai_completions") | Some("open_router") => {
            probe_openai_shaped(&client, &base, api_key, provider).await
        }
        Some("anthropic") => probe_anthropic(&client, &base, api_key).await,
        Some("gemini") => probe_gemini(&client, &base, api_key).await,
        // DeepSeek's own adapter speaks a dialect of its own, but its *HTTP*
        // surface is OpenAI-compatible, so the key can still be checked this way.
        Some("deep_seek") => probe_openai_shaped(&client, &base, api_key, provider).await,
        Some(other) => Probe::Unsupported {
            message: format!("{other} authenticates out of band, so a key cannot be tested here"),
        },
        None => Probe::Unsupported {
            message: "this provider declares no protocol".to_string(),
        },
    }
}

/// `GET {base}/models` with a bearer token — and a second opinion when the
/// endpoint turns out not to care about the token.
async fn probe_openai_shaped(
    client: &reqwest::Client,
    base: &str,
    api_key: &str,
    provider: &Provider,
) -> Probe {
    let url = format!("{base}/models");
    let response = match client.get(&url).bearer_auth(api_key).send().await {
        Ok(response) => response,
        Err(error) => return unreachable(&url, error),
    };

    match response.status() {
        status if status.is_success() => {
            let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
            let models = openai_model_ids(&body);

            // A `200` here does not prove the key is good: plenty of
            // OpenAI-compatible servers serve `/models` unauthenticated. Ask
            // again with a key that is definitely wrong — if that also passes,
            // the route proves nothing and we need a real completion.
            if !enforces_auth(client, &url).await {
                return match one_token_completion(client, base, api_key, provider).await {
                    Probe::Ok { message, .. } => Probe::Ok { models, message },
                    other => other,
                };
            }
            Probe::Ok {
                message: match models.len() {
                    0 => "the key works (this provider lists no models)".to_string(),
                    count => format!("the key works — {count} models available"),
                },
                models,
            }
        }
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => Probe::KeyRejected {
            message: "the provider rejected this key".to_string(),
        },
        reqwest::StatusCode::TOO_MANY_REQUESTS => Probe::RateLimited {
            message: "the provider is rate-limiting this key right now".to_string(),
        },
        // A 404 on `/models` is common for narrow endpoints — the key may still
        // be fine, so ask the only question that really matters.
        reqwest::StatusCode::NOT_FOUND => {
            one_token_completion(client, base, api_key, provider).await
        }
        status => Probe::Unreachable {
            message: format!("the provider answered {status}"),
        },
    }
}

/// Whether `url` actually checks the bearer token, asked by presenting one that
/// cannot possibly be valid. `true` when it refuses — which is what we want.
async fn enforces_auth(client: &reqwest::Client, url: &str) -> bool {
    match client
        .get(url)
        .bearer_auth("ic-widget-probe-invalid-key")
        .send()
        .await
    {
        Ok(response) => matches!(
            response.status(),
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ),
        // If the second call fails outright, do not claim the endpoint is
        // permissive — fall back to the completion probe, which is stricter.
        Err(_) => false,
    }
}

/// The only probe that truly validates a key: ask the model for one token.
async fn one_token_completion(
    client: &reqwest::Client,
    base: &str,
    api_key: &str,
    provider: &Provider,
) -> Probe {
    let url = format!("{base}/chat/completions");
    let response = client
        .post(&url)
        .bearer_auth(api_key)
        .json(&serde_json::json!({
            "model": provider.default_model,
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1,
        }))
        .send()
        .await;

    let response = match response {
        Ok(response) => response,
        Err(error) => return unreachable(&url, error),
    };
    match response.status() {
        status if status.is_success() => Probe::Ok {
            models: Vec::new(),
            message: "the key works (verified by a one-token completion)".to_string(),
        },
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => Probe::KeyRejected {
            message: "the provider rejected this key".to_string(),
        },
        reqwest::StatusCode::TOO_MANY_REQUESTS => Probe::RateLimited {
            message: "the provider is rate-limiting this key right now".to_string(),
        },
        // The key got through; the *model* name was wrong. That still answers
        // the question the user asked.
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::BAD_REQUEST => Probe::Ok {
            models: Vec::new(),
            message: format!(
                "the key works, but \u{201c}{}\u{201d} was not accepted as a model — pick another",
                provider.default_model
            ),
        },
        status => Probe::Unreachable {
            message: format!("the provider answered {status}"),
        },
    }
}

/// Anthropic wants its key in `x-api-key`, and a version header.
async fn probe_anthropic(client: &reqwest::Client, base: &str, api_key: &str) -> Probe {
    let url = format!("{base}/models");
    let response = client
        .get(&url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await;
    match response {
        Ok(response) => classify_listing(response, "anthropic").await,
        Err(error) => unreachable(&url, error),
    }
}

/// Gemini takes the key as a query parameter, not a header.
async fn probe_gemini(client: &reqwest::Client, base: &str, api_key: &str) -> Probe {
    let url = format!("{base}/models");
    let response = client.get(&url).query(&[("key", api_key)]).send().await;
    match response {
        Ok(response) => classify_listing(response, "gemini").await,
        Err(error) => unreachable(&url, error),
    }
}

/// The common shape of "I listed models, or I was refused".
async fn classify_listing(response: reqwest::Response, family: &str) -> Probe {
    match response.status() {
        status if status.is_success() => {
            let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
            let models = match family {
                // Anthropic: `{ "data": [ { "id": … } ] }`.
                "anthropic" => openai_model_ids(&body),
                // Gemini: `{ "models": [ { "name": "models/gemini-…" } ] }`.
                _ => body["models"]
                    .as_array()
                    .map(|models| {
                        models
                            .iter()
                            .filter_map(|model| model["name"].as_str())
                            .map(|name| name.trim_start_matches("models/").to_string())
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            Probe::Ok {
                message: match models.len() {
                    0 => "the key works (this provider listed no models)".to_string(),
                    count => format!("the key works — {count} models available"),
                },
                models,
            }
        }
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => Probe::KeyRejected {
            message: "the provider rejected this key".to_string(),
        },
        reqwest::StatusCode::TOO_MANY_REQUESTS => Probe::RateLimited {
            message: "the provider is rate-limiting this key right now".to_string(),
        },
        status => Probe::Unreachable {
            message: format!("the provider answered {status}"),
        },
    }
}

/// `{ "data": [ { "id": "gpt-…" } ] }` — the OpenAI listing shape, which
/// Anthropic also uses.
fn openai_model_ids(body: &serde_json::Value) -> Vec<String> {
    let mut ids: Vec<String> = body["data"]
        .as_array()
        .map(|models| {
            models
                .iter()
                .filter_map(|model| model["id"].as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids
}

/// Turn a transport failure into something a person can act on. The distinction
/// that matters: *we could not reach it* is a different problem from *it said no*.
fn unreachable(url: &str, error: reqwest::Error) -> Probe {
    let message = if error.is_timeout() {
        format!("{url} did not answer within {}s", TIMEOUT.as_secs())
    } else if error.is_connect() {
        format!("could not connect to {url} — check the address")
    } else {
        format!("could not reach {url}: {error}")
    };
    Probe::Unreachable { message }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A stand-in provider that answers `/models` and `/chat/completions` however
    /// the test tells it to. Lets the probe be driven end to end over real HTTP
    /// without a vendor account.
    struct FakeProvider {
        port: u16,
        _handle: tokio::task::JoinHandle<()>,
    }

    impl FakeProvider {
        /// `models_status` / `completions_status` are what the two routes answer,
        /// and `enforces_auth` decides whether a bad bearer is refused.
        async fn start(models_status: u16, completions_status: u16, enforces_auth: bool) -> Self {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let handle = tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    tokio::spawn(async move {
                        let mut buffer = [0u8; 4096];
                        let read = socket.read(&mut buffer).await.unwrap_or(0);
                        let head = String::from_utf8_lossy(&buffer[..read]).to_string();

                        let bad_key = head.contains("ic-widget-probe-invalid-key");
                        let is_models = head.contains("GET /models");

                        let (status, body) = if enforces_auth && bad_key {
                            (401, json!({"error": "bad key"}).to_string())
                        } else if is_models {
                            (
                                models_status,
                                json!({"data": [{"id": "model-b"}, {"id": "model-a"}]}).to_string(),
                            )
                        } else {
                            (
                                completions_status,
                                json!({"choices": [{"message": {"content": "hi"}}]}).to_string(),
                            )
                        };

                        let response = format!(
                            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                        let _ = socket.flush().await;
                    });
                }
            });
            FakeProvider {
                port,
                _handle: handle,
            }
        }

        fn base_url(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }
    }

    /// The catalog entry we probe against — `openai_compatible`, the escape hatch,
    /// because it takes the user's own endpoint.
    fn compatible() -> Provider {
        crate::providers::find("openai_compatible")
            .expect("decode")
            .expect("openai_compatible exists")
    }

    #[tokio::test]
    async fn a_provider_that_lists_models_and_enforces_auth_probes_ok() {
        let fake = FakeProvider::start(200, 200, true).await;
        let result = probe(&compatible(), "good-key", Some(&fake.base_url())).await;
        match result {
            Probe::Ok { models, message } => {
                assert_eq!(
                    models,
                    vec!["model-a", "model-b"],
                    "sorted, for the dropdown"
                );
                assert!(message.contains("2 models"), "{message}");
            }
            other => panic!("expected ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_rejected_key_says_so_rather_than_looking_unreachable() {
        let fake = FakeProvider::start(401, 401, true).await;
        assert!(matches!(
            probe(&compatible(), "bad-key", Some(&fake.base_url())).await,
            Probe::KeyRejected { .. }
        ));
    }

    /// THE trap this module exists for. A server that serves `/models` to anyone
    /// would make a listing probe report success for *any* key — which is exactly
    /// the lie the gateway's own probe tells. So when we detect that the endpoint
    /// does not enforce auth, we fall through to a real completion.
    #[tokio::test]
    async fn an_endpoint_that_does_not_check_the_key_falls_back_to_a_completion() {
        // `/models` answers 200 to anyone; the completion route is the one that
        // refuses the key.
        let fake = FakeProvider::start(200, 401, false).await;
        let result = probe(&compatible(), "bad-key", Some(&fake.base_url())).await;
        assert!(
            matches!(result, Probe::KeyRejected { .. }),
            "a permissive /models must not be taken as proof the key works: {result:?}"
        );
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_is_told_apart_from_a_bad_key() {
        let dead = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            drop(listener);
            port
        };
        let result = probe(
            &compatible(),
            "any-key",
            Some(&format!("http://127.0.0.1:{dead}")),
        )
        .await;
        assert!(
            matches!(result, Probe::Unreachable { .. }),
            "got {result:?} — the user must be able to tell a typo'd URL from a bad key"
        );
    }

    #[tokio::test]
    async fn a_provider_that_authenticates_out_of_band_says_it_cannot_be_tested() {
        // Copilot exchanges its token for a session; a pasted token cannot be
        // checked with a listing call, and pretending otherwise is the bug.
        let copilot = crate::providers::find("github_copilot")
            .expect("decode")
            .expect("exists");
        assert!(matches!(
            probe(&copilot, "token", None).await,
            Probe::Unsupported { .. }
        ));
    }

    #[tokio::test]
    async fn the_escape_hatch_without_an_endpoint_asks_for_one() {
        assert!(matches!(
            probe(&compatible(), "key", None).await,
            Probe::Unsupported { .. }
        ));
    }

    #[test]
    fn the_openai_listing_shape_is_read_and_sorted() {
        let models = openai_model_ids(&json!({
            "data": [{"id": "gpt-5-mini"}, {"id": "gpt-4o"}, {"no_id": true}]
        }));
        assert_eq!(models, vec!["gpt-4o", "gpt-5-mini"]);
    }

    #[test]
    fn a_body_that_is_not_a_listing_yields_no_models_rather_than_panicking() {
        assert!(openai_model_ids(&json!({"error": "nope"})).is_empty());
        assert!(openai_model_ids(&serde_json::Value::Null).is_empty());
    }

    #[test]
    fn a_probe_result_is_only_ok_when_the_key_actually_worked() {
        assert!(
            Probe::Ok {
                models: vec![],
                message: String::new()
            }
            .is_ok()
        );
        assert!(
            !Probe::KeyRejected {
                message: String::new()
            }
            .is_ok()
        );
        assert!(
            !Probe::Unsupported {
                message: String::new()
            }
            .is_ok()
        );
    }
}
