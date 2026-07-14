//! Test harness for the Phase 0 upstream-merge gate.
//!
//! Everything the round-trip gate test needs to stand up a *hermetic*
//! `ironclaw-reborn serve` instance and talk to its WebChat v2 API:
//!
//! - [`MockLlm`] — a minimal HTTP server that answers the OpenAI-compatible
//!   Chat Completions endpoint (`POST /v1/chat/completions`) with a single
//!   canned assistant reply. `openai_compatible` goes through `RigAdapter`,
//!   which uses the **non-streaming** Chat Completions API, so a fixed JSON
//!   body is all that is required to drive the agent loop to a final answer.
//! - [`RebornServer`] — spawns the `ironclaw-reborn serve` binary against the
//!   libSQL `local-dev` profile, wired to the mock LLM via the generic
//!   `LLM_BACKEND=openai_compatible` env contract, and polls it to readiness.
//!
//! The gate test lives in `tests/chat_roundtrip.rs`. See the crate `Cargo.toml`
//! for how CI builds the `serve` binary before running it.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use uuid::Uuid;

/// The API base path prefix every WebChat v2 route is mounted under.
pub const API_PREFIX: &str = "/api/webchat/v2";

/// The runtime's default owner identity. `IRONCLAW_REBORN_WEBUI_USER_ID` must
/// equal `[identity].default_owner` or WebUI-created threads are invisible to
/// the turn runner (verified during the Phase 0 Windows smoke).
pub const OWNER_USER_ID: &str = "reborn-cli";

/// Locate the `ironclaw-reborn` binary.
///
/// A separate test crate cannot use `env!("CARGO_BIN_EXE_ironclaw-reborn")`
/// (that is only defined for the crate that declares the binary), so we resolve
/// it from the test executable's own location: Cargo places integration-test
/// binaries in `target/<profile>/deps/`, with workspace binaries one directory
/// up in `target/<profile>/`. An `IRONCLAW_REBORN_BIN` override wins when set.
pub fn reborn_bin() -> PathBuf {
    if let Ok(path) = std::env::var("IRONCLAW_REBORN_BIN") {
        return PathBuf::from(path);
    }
    let mut dir = std::env::current_exe().expect("current_exe should resolve");
    dir.pop(); // drop the test executable file name -> .../deps/
    if dir.ends_with("deps") {
        dir.pop(); // -> .../<profile>/
    }
    let name = if cfg!(windows) {
        "ironclaw-reborn.exe"
    } else {
        "ironclaw-reborn"
    };
    dir.join(name)
}

/// Reserve a free localhost TCP port by binding to `:0` and reading the
/// assigned port back. The listener is dropped immediately; there is a small
/// race window before `serve` rebinds, which is the documented approach —
/// `serve` with `--port 0` never reports the ephemeral port it bound, so the
/// caller must choose a concrete port up front.
pub fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local_addr").port()
}

/// The `IRONCLAW_REBORN_HOME` a server started over `home_root` uses. Lets a
/// restart probe locate on-disk state — installed skills live under
/// `<reborn home>/local-dev/skills/<name>/SKILL.md`.
pub fn reborn_home_dir(home_root: &Path) -> PathBuf {
    home_root.join("reborn-home")
}

/// What the mock LLM answers one Chat Completions request with.
///
/// A plain [`MockReply::Text`] ends the turn. A [`MockReply::ToolCall`] makes the
/// agent execute a real tool and come back for another completion — which is the
/// only way a test can reach a capability the agent alone can invoke (there is no
/// HTTP route that creates a trigger; see `docs/desktop/dashboard-gaps.md`).
#[derive(Debug, Clone)]
pub enum MockReply {
    /// A final assistant message.
    Text(String),
    /// A tool call. `name` is the **model-visible** tool name (capability id with
    /// `.` folded to `__`, e.g. `builtin__trigger_create`).
    ToolCall {
        /// Model-visible tool name.
        name: String,
        /// The tool arguments, serialized into the OpenAI `arguments` string.
        arguments: serde_json::Value,
    },
}

/// Decides what the mock answers, given the raw request body IronClaw sent.
///
/// Content-conditioned rather than a fixed script, because more than one thread
/// (chat, ambient, a trigger-fired run) can be in flight against the same mock and
/// a positional script would depend on their interleaving.
pub type MockResponder = Arc<dyn Fn(&str) -> MockReply + Send + Sync>;

/// A minimal OpenAI-compatible mock LLM server.
pub struct MockLlm {
    /// The localhost port the mock is listening on.
    pub port: u16,
    handle: tokio::task::JoinHandle<()>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl MockLlm {
    /// Start the mock, answering every Chat Completions request with `answer`.
    pub async fn start(answer: String) -> MockLlm {
        let reply = MockReply::Text(answer);
        Self::start_responding(Arc::new(move |_| reply.clone())).await
    }

    /// Start the mock with a responder that inspects each request body.
    pub async fn start_responding(responder: MockResponder) -> MockLlm {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock llm");
        let port = listener.local_addr().expect("mock local_addr").port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&requests);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let responder = Arc::clone(&responder);
                let sink = Arc::clone(&sink);
                tokio::spawn(async move {
                    // Best-effort: a broken connection just means the client
                    // gave up; nothing for the test to do about it.
                    let _ = serve_mock_connection(socket, responder, sink).await;
                });
            }
        });
        MockLlm {
            port,
            handle,
            requests,
        }
    }

    /// The base URL to hand to `LLM_BASE_URL` (includes the `/v1` suffix the
    /// OpenAI-compatible client expects).
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// The raw JSON bodies of every `POST /v1/chat/completions` seen so far, in
    /// order. Lets a test assert on what IronClaw actually sends a provider —
    /// the tool schemas, the system prompt, the message history.
    pub fn chat_requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

impl Drop for MockLlm {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Read one full HTTP/1.1 request (headers + any `Content-Length` body) and
/// reply with a canned JSON response. Reading the whole request before
/// responding avoids a request-side broken-pipe error on the client.
async fn serve_mock_connection(
    mut socket: tokio::net::TcpStream,
    responder: MockResponder,
    requests: Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];

    // Read until the end of the headers.
    let header_end = loop {
        if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = socket.read(&mut chunk).await?;
        if n == 0 {
            return Ok(()); // client closed before sending a full request
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 1024 * 1024 {
            return Ok(()); // runaway; bail
        }
    };

    let headers = String::from_utf8_lossy(&buf[..header_end]);
    let request_line = headers.lines().next().unwrap_or_default().to_string();
    let content_length = parse_content_length(&headers);

    // Read the body to completion so the client's write side finishes cleanly,
    // and keep it: it is the only place a test can see what IronClaw sends a
    // provider.
    while buf.len() - header_end < content_length {
        let n = socket.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    let is_chat = request_line.contains("chat/completions");
    let request_body = String::from_utf8_lossy(&buf[header_end..]).into_owned();
    if is_chat && let Ok(mut guard) = requests.lock() {
        guard.push(request_body.clone());
    }

    let body = if is_chat {
        chat_completion_json(&responder(&request_body))
    } else {
        // `/v1/models` and anything else the provider probes.
        models_json()
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    socket.write_all(response.as_bytes()).await?;
    socket.flush().await?;
    let _ = socket.shutdown().await;
    Ok(())
}

fn chat_completion_json(reply: &MockReply) -> String {
    let (message, finish_reason) = match reply {
        MockReply::Text(answer) => (
            serde_json::json!({ "role": "assistant", "content": answer }),
            "stop",
        ),
        MockReply::ToolCall { name, arguments } => (
            serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call-mock-1",
                    "type": "function",
                    "function": { "name": name, "arguments": arguments.to_string() }
                }]
            }),
            "tool_calls",
        ),
    };
    serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "created": 0,
        "model": "mock-model",
        "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    })
    .to_string()
}

fn models_json() -> String {
    serde_json::json!({
        "object": "list",
        "data": [{ "id": "mock-model", "object": "model", "owned_by": "mock" }]
    })
    .to_string()
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_content_length(headers: &str) -> usize {
    for line in headers.lines() {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            return value.trim().parse().unwrap_or(0);
        }
    }
    0
}

/// Either of the child's output pipes, so both can be drained by one loop.
enum PipeOut {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

impl std::io::Read for PipeOut {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            PipeOut::Out(pipe) => pipe.read(buf),
            PipeOut::Err(pipe) => pipe.read(buf),
        }
    }
}

/// A running `ironclaw-reborn serve` instance.
///
/// The child process is killed and reaped on drop.
pub struct RebornServer {
    child: Child,
    /// e.g. `http://127.0.0.1:38080`.
    pub base_url: String,
    /// The bearer token accepted by this instance.
    pub token: String,
    /// The exact assistant text the mock LLM replies with; the gate test
    /// asserts this string surfaces in the SSE projection stream. Empty when
    /// the server is wired to a real provider via [`RebornServer::start_with_llm`].
    pub answer: String,
    stderr: Arc<Mutex<String>>,
    /// Kept alive for the lifetime of the server. `None` when a real provider
    /// is in use.
    _mock: Option<MockLlm>,
    /// The server's on-disk state, deleted on drop. `None` when the caller owns
    /// the home directory (a restart probe reusing one home across two servers).
    _home: Option<tempfile::TempDir>,
    client: reqwest::Client,
}

impl RebornServer {
    /// Spawn `serve` against the libSQL `local-dev` profile with a hermetic
    /// mock LLM and poll it to readiness. Panics with the captured serve
    /// stderr if the process exits or never becomes ready.
    pub async fn start() -> RebornServer {
        let answer = format!("icinteg-ok-{}", Uuid::new_v4().simple());
        let mock = MockLlm::start(answer.clone()).await;
        Self::start_with_mock(mock, answer, Vec::new(), None).await
    }

    /// Spawn `serve` against a mock LLM you built yourself (so the test can drive
    /// tool calls), plus any extra environment the runtime should see.
    ///
    /// `answer` is only what [`RebornServer::answer`] reports; the mock's own
    /// responder decides what it actually replies with.
    pub async fn start_scripted(
        responder: MockResponder,
        answer: String,
        extra_env: Vec<(String, String)>,
    ) -> RebornServer {
        let mock = MockLlm::start_responding(responder).await;
        Self::start_with_mock(mock, answer, extra_env, None).await
    }

    /// Like [`RebornServer::start_scripted`], but over a caller-owned home
    /// directory instead of a fresh tempdir. Two consecutive servers over the
    /// same directory share all on-disk state (the libSQL store, installed
    /// skills) — the only way a test can observe what survives a gateway
    /// restart. Drop the first server before starting the second: it holds the
    /// libSQL write lock and the home is not built for two writers.
    pub async fn start_scripted_in_home(
        responder: MockResponder,
        answer: String,
        extra_env: Vec<(String, String)>,
        home_root: &Path,
    ) -> RebornServer {
        let mock = MockLlm::start_responding(responder).await;
        Self::start_with_mock(mock, answer, extra_env, Some(home_root.to_path_buf())).await
    }

    async fn start_with_mock(
        mock: MockLlm,
        answer: String,
        extra_env: Vec<(String, String)>,
        external_home: Option<PathBuf>,
    ) -> RebornServer {
        // Force the hermetic mock; `LLM_BACKEND` set means no other provider env
        // (a developer's real keys) is consulted.
        let mut env = vec![
            ("LLM_BACKEND".to_string(), "openai_compatible".to_string()),
            ("LLM_BASE_URL".to_string(), mock.base_url()),
            ("LLM_API_KEY".to_string(), "test-key".to_string()),
            ("LLM_MODEL".to_string(), "mock-model".to_string()),
        ];
        env.extend(extra_env);
        Self::start_inner(env, Some(mock), answer, external_home).await
    }

    /// Spawn `serve` wired to whatever provider `llm_env` describes — in
    /// practice, the four variables `ic_llama::wiring::LlmEnv` emits for a local
    /// `llama-server`.
    ///
    /// [`RebornServer::answer`] is empty here: a real model's reply is not known
    /// ahead of time, so callers assert on its content instead.
    pub async fn start_with_llm(llm_env: Vec<(String, String)>) -> RebornServer {
        Self::start_inner(llm_env, None, String::new(), None).await
    }

    async fn start_inner(
        llm_env: Vec<(String, String)>,
        mock: Option<MockLlm>,
        answer: String,
        external_home: Option<PathBuf>,
    ) -> RebornServer {
        let bin = reborn_bin();
        assert!(
            bin.exists(),
            "ironclaw-reborn binary not found at {}.\nBuild it first:\n  \
             cargo build -p ironclaw_reborn_cli --bin ironclaw-reborn --features webui-v2-beta\n\
             or set IRONCLAW_REBORN_BIN to its path.",
            bin.display()
        );

        let (home_root, home_guard) = match external_home {
            Some(root) => (root, None),
            None => {
                let dir = tempfile::tempdir().expect("tempdir");
                (dir.path().to_path_buf(), Some(dir))
            }
        };
        // Any non-empty token works for the single-operator env-bearer auth
        // (SSO, which imposes a 32-byte minimum, is not enabled here).
        let token = format!("icinteg-token-{}", Uuid::new_v4().simple());
        let port = free_port();
        let base_url = format!("http://127.0.0.1:{port}");

        let mut command = Command::new(&bin);
        command
            .arg("serve")
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            // Isolate all on-disk state to the home root.
            .env("IRONCLAW_REBORN_HOME", reborn_home_dir(&home_root))
            .env("HOME", home_root.join("home"))
            .env("USERPROFILE", home_root.join("home"))
            .env("IRONCLAW_REBORN_PROFILE", "local-dev")
            // Single-operator env-bearer auth.
            .env("IRONCLAW_REBORN_WEBUI_TOKEN", &token)
            .env("IRONCLAW_REBORN_WEBUI_USER_ID", OWNER_USER_ID)
            // Keep the loop fast and deterministic on provider failures.
            .env("LLM_MAX_RETRIES", "0")
            .envs(llm_env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn().expect("spawn ironclaw-reborn serve");

        // Drain **both** pipes into one buffer. `serve` writes its tracing output
        // to stdout, not stderr, so draining only stderr left the log empty at
        // exactly the moments it was needed (a hung tool call says nothing at
        // all). Both are diagnostics; one buffer is what a reader wants.
        let stderr = Arc::new(Mutex::new(String::new()));
        for pipe in [
            child.stderr.take().map(PipeOut::Err),
            child.stdout.take().map(PipeOut::Out),
        ]
        .into_iter()
        .flatten()
        {
            let sink = Arc::clone(&stderr);
            std::thread::spawn(move || {
                let mut pipe = pipe;
                let mut chunk = [0u8; 4096];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut guard) = sink.lock() {
                                guard.push_str(&String::from_utf8_lossy(&chunk[..n]));
                            }
                        }
                    }
                }
            });
        }

        // Poll to readiness while we still own `child` mutably so we can fail
        // fast if the process exits during boot. Readiness = the authenticated
        // thread-list route returns 200 (listener bound + auth wired + runtime
        // ready). There is no dedicated `/health` route.
        let client = reqwest::Client::new();
        let readiness_url = format!("{base_url}{API_PREFIX}/threads");
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Some(status) = child.try_wait().expect("try_wait on serve") {
                let captured = stderr.lock().map(|g| g.clone()).unwrap_or_default();
                let _ = child.wait();
                panic!(
                    "serve exited during startup with {status:?}.\n--- serve stderr ---\n{captured}"
                );
            }
            let ready = client
                .get(&readiness_url)
                .bearer_auth(&token)
                .timeout(Duration::from_secs(5))
                .send()
                .await
                .map(|response| response.status().is_success())
                .unwrap_or(false);
            if ready {
                break;
            }
            if Instant::now() >= deadline {
                let captured = stderr.lock().map(|g| g.clone()).unwrap_or_default();
                let _ = child.kill();
                let _ = child.wait();
                panic!("serve did not become ready within 90s.\n--- serve stderr ---\n{captured}");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        RebornServer {
            child,
            base_url,
            token,
            answer,
            stderr,
            _mock: mock,
            _home: home_guard,
            client,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}{}", self.base_url, API_PREFIX, path)
    }

    /// The raw JSON bodies IronClaw sent the mock provider, in order. Empty when
    /// the server is wired to a real provider.
    pub fn chat_requests(&self) -> Vec<String> {
        self._mock
            .as_ref()
            .map(MockLlm::chat_requests)
            .unwrap_or_default()
    }

    /// The mock provider's base URL, when this server runs one. Lets a test
    /// point the gateway's own provider probes at a provider it controls.
    pub fn llm_base_url(&self) -> Option<String> {
        self._mock.as_ref().map(MockLlm::base_url)
    }

    /// Snapshot the serve process stderr captured so far.
    pub fn stderr_snapshot(&self) -> String {
        self.stderr
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Fetch the raw timeline (message history) JSON for a thread.
    pub async fn timeline(&self, thread_id: &str) -> serde_json::Value {
        let response = self
            .client
            .get(self.url(&format!("/threads/{thread_id}/timeline")))
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("timeline request");
        let status = response.status();
        let body: serde_json::Value = response.json().await.expect("timeline json");
        assert!(status.is_success(), "timeline failed ({status}): {body}");
        body
    }

    /// The raw timeline body with paging — `{ messages: [...], next_cursor? }`.
    /// Note `next_cursor` is **omitted** (not null) when there is no next page.
    pub async fn timeline_raw(
        &self,
        thread_id: &str,
        limit: Option<u32>,
        cursor: Option<&str>,
    ) -> serde_json::Value {
        let mut request = self
            .client
            .get(self.url(&format!("/threads/{thread_id}/timeline")))
            .bearer_auth(&self.token);
        if let Some(limit) = limit {
            request = request.query(&[("limit", limit.to_string())]);
        }
        if let Some(cursor) = cursor {
            request = request.query(&[("cursor", cursor)]);
        }
        let response = request.send().await.expect("timeline request");
        let status = response.status();
        let body: serde_json::Value = response.json().await.expect("timeline json");
        assert!(status.is_success(), "timeline failed ({status}): {body}");
        body
    }

    /// Poll the timeline until `needle` appears in it or `timeout` elapses.
    /// The assistant reply is persisted as a thread message and read back from
    /// the timeline (it is not carried as a `text` item in the projection SSE
    /// stream, which only surfaces `run_status` transitions). Returns the last
    /// timeline observed and whether the needle was found.
    pub async fn wait_for_timeline_text(
        &self,
        thread_id: &str,
        needle: &str,
        timeout: Duration,
    ) -> (bool, serde_json::Value) {
        let deadline = Instant::now() + timeout;
        loop {
            let timeline = self.timeline(thread_id).await;
            if timeline.to_string().contains(needle) {
                return (true, timeline);
            }
            if Instant::now() >= deadline {
                return (false, timeline);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Every thread id `GET /threads` reports for the authenticated caller.
    ///
    /// A trigger-fired run lands in a thread the *poller* created, not one the
    /// caller created, so this is the only way the widget can find it: automations
    /// carry no thread id or run id on the wire.
    pub async fn thread_ids(&self) -> Vec<String> {
        let response = self
            .client
            .get(self.url("/threads"))
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("list threads request");
        let status = response.status();
        let body: serde_json::Value = response.json().await.expect("threads json");
        assert!(
            status.is_success(),
            "list threads failed ({status}): {body}"
        );
        body["threads"]
            .as_array()
            .map(|threads| {
                threads
                    .iter()
                    .filter_map(|thread| thread["thread_id"].as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The raw `GET /threads` body, with optional paging — the shape the Chats
    /// panel reads (`{ threads: [...], next_cursor: "…"|null }`).
    pub async fn threads_raw(&self, limit: Option<u32>, cursor: Option<&str>) -> serde_json::Value {
        let mut request = self
            .client
            .get(self.url("/threads"))
            .bearer_auth(&self.token);
        if let Some(limit) = limit {
            request = request.query(&[("limit", limit.to_string())]);
        }
        if let Some(cursor) = cursor {
            request = request.query(&[("cursor", cursor)]);
        }
        let response = request.send().await.expect("list threads request");
        let status = response.status();
        let body: serde_json::Value = response.json().await.expect("threads json");
        assert!(
            status.is_success(),
            "list threads failed ({status}): {body}"
        );
        body
    }

    /// Cancel a run — the Stop button. Panics on a non-success status; use
    /// [`RebornServer::cancel_run_raw`] to inspect the failure modes.
    pub async fn cancel_run(&self, thread_id: &str, run_id: &str) -> serde_json::Value {
        let (status, body) = self.cancel_run_raw(thread_id, run_id).await;
        assert!(status.is_success(), "cancel failed ({status}): {body}");
        body
    }

    /// Cancel a run, returning the status alongside the body — for the races the
    /// Stop button actually lives in (already-terminal, unknown run).
    pub async fn cancel_run_raw(
        &self,
        thread_id: &str,
        run_id: &str,
    ) -> (reqwest::StatusCode, serde_json::Value) {
        let response = self
            .client
            .post(self.url(&format!("/threads/{thread_id}/runs/{run_id}/cancel")))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "client_action_id": Uuid::new_v4().to_string(),
                "reason": "user_requested",
            }))
            .send()
            .await
            .expect("cancel request");
        let status = response.status();
        let body = response.json().await.unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    /// The raw `GET /automations` body.
    pub async fn automations(&self) -> serde_json::Value {
        let response = self
            .client
            .get(self.url("/automations"))
            .bearer_auth(&self.token)
            .send()
            .await
            .expect("list automations request");
        let status = response.status();
        let body: serde_json::Value = response.json().await.expect("automations json");
        assert!(
            status.is_success(),
            "list automations failed ({status}): {body}"
        );
        body
    }

    /// Poll `GET /threads` until a thread id appears that is not in `known`, or
    /// `timeout` elapses. Returns the new thread id.
    pub async fn wait_for_new_thread(&self, known: &[String], timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let found = self
                .thread_ids()
                .await
                .into_iter()
                .find(|id| !known.contains(id));
            if found.is_some() {
                return found;
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Create a thread; returns its `thread_id`.
    pub async fn create_thread(&self) -> String {
        let response = self
            .client
            .post(self.url("/threads"))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "client_action_id": Uuid::new_v4().to_string() }))
            .send()
            .await
            .expect("create_thread request");
        let status = response.status();
        let body: serde_json::Value = response.json().await.expect("create_thread json");
        assert!(
            status.is_success(),
            "create_thread failed ({status}): {body}"
        );
        body["thread"]["thread_id"]
            .as_str()
            .unwrap_or_else(|| panic!("thread.thread_id missing in {body}"))
            .to_string()
    }

    /// Send a user message; returns the `run_id` of the submitted turn.
    pub async fn send_message(&self, thread_id: &str, content: &str) -> String {
        let response = self
            .client
            .post(self.url(&format!("/threads/{thread_id}/messages")))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "client_action_id": Uuid::new_v4().to_string(),
                "content": content,
            }))
            .send()
            .await
            .expect("send_message request");
        let status = response.status();
        let body: serde_json::Value = response.json().await.expect("send_message json");
        assert!(
            status.is_success(),
            "send_message failed ({status}): {body}"
        );
        assert_eq!(
            body["outcome"], "submitted",
            "expected outcome=submitted, got {body}"
        );
        body["run_id"]
            .as_str()
            .unwrap_or_else(|| panic!("run_id missing in {body}"))
            .to_string()
    }

    /// Open the SSE event stream and accumulate frames until `needle` appears
    /// or `timeout` elapses. Returns the accumulated stream text and whether
    /// the needle was seen.
    pub async fn stream_until(
        &self,
        thread_id: &str,
        needle: &str,
        timeout: Duration,
    ) -> (bool, String) {
        let url = format!(
            "{}?token={}",
            self.url(&format!("/threads/{thread_id}/events")),
            self.token
        );
        let accumulated = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&accumulated);
        let request = self.client.get(url).header("Accept", "text/event-stream");

        let found = tokio::time::timeout(timeout, async move {
            let response = request.send().await.expect("open sse stream");
            assert!(
                response.status().is_success(),
                "sse stream returned {}",
                response.status()
            );
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.expect("sse chunk");
                let text = String::from_utf8_lossy(&chunk);
                let contains = {
                    let mut guard = sink.lock().expect("sse buffer lock");
                    guard.push_str(&text);
                    guard.contains(needle)
                };
                if contains {
                    return;
                }
            }
        })
        .await
        .is_ok();

        let text = accumulated.lock().map(|g| g.clone()).unwrap_or_default();
        (found, text)
    }
}

impl Drop for RebornServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
