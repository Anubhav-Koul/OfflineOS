//! The schema proxy, driven over real HTTP against a recording upstream.
//!
//! `src/proxy.rs` unit-tests the sanitizer as a pure function. These tests cover
//! what only shows up on the wire: that the *upstream* receives the repaired
//! body, that everything which is not a chat completion passes through
//! untouched, that headers survive, and that a dead `llama-server` becomes a
//! `502` rather than a hang.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ic_llama::SchemaProxy;
use ic_llama::proxy::MAX_GRAMMAR_REPETITIONS;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// One request as the upstream saw it.
#[derive(Clone, Debug)]
struct Seen {
    request_line: String,
    headers: String,
    body: String,
}

/// A recording stand-in for `llama-server`.
struct Upstream {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl Upstream {
    async fn start() -> Upstream {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream");
        let port = listener.local_addr().expect("addr").port();
        let seen = Arc::new(Mutex::new(Vec::new()));

        let sink = Arc::clone(&seen);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let sink = Arc::clone(&sink);
                tokio::spawn(async move {
                    let _ = serve(socket, sink).await;
                });
            }
        });
        Upstream { port, seen, handle }
    }

    fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().expect("request log").clone()
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn serve(
    mut socket: tokio::net::TcpStream,
    sink: Arc<Mutex<Vec<Seen>>>,
) -> std::io::Result<()> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 2048];
    let header_end = loop {
        if let Some(at) = find(&buffer, b"\r\n\r\n") {
            break at + 4;
        }
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let headers = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);

    while buffer.len() - header_end < content_length {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }

    sink.lock().expect("request log").push(Seen {
        request_line: headers.lines().next().unwrap_or_default().to_string(),
        headers: headers.clone(),
        body: String::from_utf8_lossy(&buffer[header_end..]).into_owned(),
    });

    let body = r#"{"ok":true}"#;
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Upstream: yes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.flush().await?;
    let _ = socket.shutdown().await;
    Ok(())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The payload IronClaw actually sends, reduced to the field that breaks.
fn spawn_subagent_payload() -> serde_json::Value {
    serde_json::json!({
        "model": "Qwen3-4B-Q4_K_M",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{
            "type": "function",
            "function": {
                "name": "builtin__spawn_subagent",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task": {"type": "string", "maxLength": 65536},
                        "handoff": {"type": "string", "maxLength": 65536},
                        "agent": {"type": "string", "maxLength": 64}
                    }
                }
            }
        }]
    })
}

#[tokio::test]
async fn the_upstream_receives_a_body_llama_cpp_can_compile() {
    let upstream = Upstream::start().await;
    let proxy = SchemaProxy::start(upstream.origin()).await.expect("proxy");

    let response = reqwest::Client::new()
        .post(format!("{}/chat/completions", proxy.base_url()))
        .json(&spawn_subagent_payload())
        .send()
        .await
        .expect("proxied request");
    assert!(response.status().is_success());

    let seen = upstream.requests();
    assert_eq!(seen.len(), 1);
    let body: serde_json::Value = serde_json::from_str(&seen[0].body).expect("upstream got json");
    let properties = &body["tools"][0]["function"]["parameters"]["properties"];

    // The two bounds that break the grammar are gone...
    assert!(properties["task"].get("maxLength").is_none());
    assert!(properties["handoff"].get("maxLength").is_none());
    // ...the one llama.cpp can compile survives...
    assert_eq!(properties["agent"]["maxLength"], 64);
    // ...and nothing else was disturbed.
    assert_eq!(body["model"], "Qwen3-4B-Q4_K_M");
    assert_eq!(body["messages"][0]["content"], "hi");
    assert_eq!(
        body["tools"][0]["function"]["name"],
        "builtin__spawn_subagent"
    );
}

#[tokio::test]
async fn the_rewritten_body_carries_a_matching_content_length() {
    let upstream = Upstream::start().await;
    let proxy = SchemaProxy::start(upstream.origin()).await.expect("proxy");

    reqwest::Client::new()
        .post(format!("{}/chat/completions", proxy.base_url()))
        .json(&spawn_subagent_payload())
        .send()
        .await
        .expect("proxied request");

    let seen = upstream.requests();
    let declared = seen[0]
        .headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .expect("a content-length");
    // The body shrank when the bounds were stripped; a stale length would make
    // the upstream hang waiting for bytes that never come.
    assert_eq!(declared, seen[0].body.len());
    assert!(!seen[0].headers.to_lowercase().contains("transfer-encoding"));
}

#[tokio::test]
async fn requests_that_are_not_chat_completions_pass_through_untouched() {
    let upstream = Upstream::start().await;
    let proxy = SchemaProxy::start(upstream.origin()).await.expect("proxy");

    let response = reqwest::Client::new()
        .get(format!("{}/models", proxy.base_url()))
        .send()
        .await
        .expect("proxied request");
    assert!(response.status().is_success());
    // Upstream response headers reach the client.
    assert_eq!(
        response.headers().get("x-upstream").map(|v| v.as_bytes()),
        Some("yes".as_bytes())
    );
    assert_eq!(response.text().await.expect("body"), r#"{"ok":true}"#);

    let seen = upstream.requests();
    assert_eq!(seen[0].request_line, "GET /v1/models HTTP/1.1");
}

#[tokio::test]
async fn the_authorization_header_reaches_llama_server() {
    let upstream = Upstream::start().await;
    let proxy = SchemaProxy::start(upstream.origin()).await.expect("proxy");

    reqwest::Client::new()
        .post(format!("{}/chat/completions", proxy.base_url()))
        .bearer_auth("ic-secret-token")
        .json(&spawn_subagent_payload())
        .send()
        .await
        .expect("proxied request");

    // `--api-key` on the sidecar is what keeps other local processes out; the
    // proxy must not swallow the credential that satisfies it.
    let seen = upstream.requests();
    assert!(
        seen[0].headers.contains("Bearer ic-secret-token"),
        "{}",
        seen[0].headers
    );
    // The Host header must name the upstream, not the proxy.
    assert!(
        seen[0]
            .headers
            .contains(&format!("127.0.0.1:{}", upstream.port)),
        "{}",
        seen[0].headers
    );
}

#[tokio::test]
async fn a_chat_body_that_is_not_json_is_forwarded_rather_than_rejected() {
    let upstream = Upstream::start().await;
    let proxy = SchemaProxy::start(upstream.origin()).await.expect("proxy");

    reqwest::Client::new()
        .post(format!("{}/chat/completions", proxy.base_url()))
        .header("content-type", "application/json")
        .body("this is not json")
        .send()
        .await
        .expect("proxied request");

    // Let llama-server produce the error; the proxy is not a validator.
    assert_eq!(upstream.requests()[0].body, "this is not json");
}

#[tokio::test]
async fn a_body_needing_no_repair_reaches_the_upstream_unchanged() {
    let upstream = Upstream::start().await;
    let proxy = SchemaProxy::start(upstream.origin()).await.expect("proxy");

    let payload = serde_json::json!({
        "model": "m",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{"function": {"parameters": {"properties": {
            "at_the_limit": {"type": "string", "maxLength": MAX_GRAMMAR_REPETITIONS}
        }}}}]
    });
    reqwest::Client::new()
        .post(format!("{}/chat/completions", proxy.base_url()))
        .json(&payload)
        .send()
        .await
        .expect("proxied request");

    let body: serde_json::Value = serde_json::from_str(&upstream.requests()[0].body).expect("json");
    assert_eq!(body, payload);
}

#[tokio::test]
async fn a_dead_llama_server_becomes_a_502_rather_than_a_hang() {
    // Nothing is listening on this port.
    let proxy = SchemaProxy::start("http://127.0.0.1:1")
        .await
        .expect("proxy");

    let response = tokio::time::timeout(
        Duration::from_secs(10),
        reqwest::Client::new()
            .post(format!("{}/chat/completions", proxy.base_url()))
            .json(&spawn_subagent_payload())
            .send(),
    )
    .await
    .expect("the proxy must not hang")
    .expect("a response");

    assert_eq!(response.status(), reqwest::StatusCode::BAD_GATEWAY);
    let body: serde_json::Value = response.json().await.expect("json error body");
    assert_eq!(body["error"]["code"], 502);
}

#[tokio::test]
async fn dropping_the_proxy_stops_the_listener() {
    let upstream = Upstream::start().await;
    let proxy = SchemaProxy::start(upstream.origin()).await.expect("proxy");
    let url = format!("{}/models", proxy.base_url());
    assert!(
        reqwest::get(&url)
            .await
            .expect("live")
            .status()
            .is_success()
    );

    drop(proxy);

    // The listener task is aborted, so the port stops accepting. Poll: the abort
    // is asynchronous.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if reqwest::get(&url).await.is_err() {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "proxy still serving");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
