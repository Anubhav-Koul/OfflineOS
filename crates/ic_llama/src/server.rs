//! Supervising the `llama-server` child process.
//!
//! The sidecar owns one `llama-server` process and keeps it alive across
//! crashes. Three details are worth knowing:
//!
//! **The port is chosen once and reused for every restart.** IronClaw is handed
//! `LLM_BASE_URL` when it starts and never re-reads it, so a restart that landed
//! on a different port would silently break inference. The port is reserved at
//! [`SidecarConfig::new`] and every respawn binds it again.
//!
//! **A model that keeps taking the server down is marked suspect rather than
//! restarted forever.** Two consecutive failures (an out-of-memory `-ngl`, a
//! corrupt GGUF, an unsupported quantization) produce the same failure on the
//! third attempt, and an endless restart loop burns the user's battery while
//! looking like a hang.
//!
//! **Liveness is not readiness.** `llama-server` binds its port before the model
//! is loaded and answers `GET /health` with `503 Loading model` until the weights
//! are resident, which for a large model on a cold page cache is minutes. A
//! process that is up but never reaches `200 OK` within the startup budget is
//! killed and counted as a failure, since it is wedged rather than working.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::error::{Error, Result};
use crate::ids::ModelId;

/// How often the supervisor checks `/health` and whether the child is still
/// alive.
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Lines of child output kept for diagnostics.
const OUTPUT_TAIL_LINES: usize = 200;

/// How long a server must stay healthy before its earlier crashes stop counting
/// against the model. Without this, a server that runs fine for hours and then
/// dies twice over a week would be marked suspect.
const DEFAULT_STABILITY_WINDOW: Duration = Duration::from_secs(60);

/// Added to [`SidecarConfig::startup_budget`] so the supervisor reaches its own
/// verdict before the caller's deadline fires. See that method.
const SUPERVISOR_SLACK: Duration = Duration::from_secs(5);

/// What the sidecar is doing right now. Surfaced to the widget as a health
/// badge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum SidecarState {
    /// The process has been spawned but has not answered `/health` yet.
    Starting,
    /// The process is up and loading the model into memory.
    Loading,
    /// The server is answering requests.
    Ready,
    /// The process died and will be respawned.
    Restarting {
        /// Which consecutive failure this is.
        attempt: u32,
        /// How long until the next spawn.
        backoff_ms: u64,
    },
    /// The process failed too many times in a row. Terminal: no further
    /// restarts happen without a new [`Sidecar::start`].
    Suspect {
        /// User-facing explanation, including the tail of the server's output.
        reason: String,
    },
    /// Shut down at the caller's request. Terminal.
    Stopped,
}

impl SidecarState {
    /// Whether the server can serve inference requests right now.
    pub fn is_ready(&self) -> bool {
        matches!(self, SidecarState::Ready)
    }

    /// Whether the supervisor has given up, one way or another.
    pub fn is_terminal(&self) -> bool {
        matches!(self, SidecarState::Suspect { .. } | SidecarState::Stopped)
    }
}

/// How to launch `llama-server`.
#[derive(Debug, Clone)]
pub struct SidecarConfig {
    /// Path to the `llama-server` binary from [`crate::runtime::LlamaRuntime`].
    pub server_bin: PathBuf,
    /// The GGUF file to load.
    pub model_path: PathBuf,
    /// The name the model answers to over the OpenAI API.
    pub model_id: ModelId,
    /// `-ngl`, from [`crate::placement::plan`].
    pub n_gpu_layers: u32,
    /// `-c`, the context window.
    pub ctx_size: u32,
    /// Bearer token the server requires. Loopback binding already keeps remote
    /// hosts out; this keeps *other local processes* out, which on a shared
    /// desktop is the threat that actually exists.
    pub api_key: String,
    /// The loopback port, held stable across restarts.
    pub port: u16,
    /// Extra `llama-server` flags, appended verbatim.
    pub extra_args: Vec<String>,
    /// Extra environment for the child, on top of the inherited environment.
    /// This is how a device is pinned (`CUDA_VISIBLE_DEVICES`,
    /// `GGML_VK_VISIBLE_DEVICES`) on a machine with more than one GPU.
    pub env: Vec<(String, String)>,
    /// How long one spawn has to reach `200 OK` on `/health`.
    pub startup_timeout: Duration,
    /// Consecutive failures before the model is declared suspect.
    pub max_crashes: u32,
    /// How long a server must stay healthy to reset the failure count.
    pub stability_window: Duration,
    /// Delay before the first restart; doubles each subsequent attempt.
    pub initial_backoff: Duration,
    /// Ceiling on the restart delay.
    pub max_backoff: Duration,
}

impl SidecarConfig {
    /// Build a config with a freshly reserved loopback port and a random API
    /// key.
    pub fn new(server_bin: PathBuf, model_path: PathBuf, model_id: ModelId) -> Result<Self> {
        Ok(Self {
            server_bin,
            model_path,
            model_id,
            n_gpu_layers: 0,
            ctx_size: 4096,
            api_key: random_api_key(),
            port: free_port()?,
            extra_args: Vec::new(),
            env: Vec::new(),
            startup_timeout: Duration::from_secs(300),
            max_crashes: 2,
            stability_window: DEFAULT_STABILITY_WINDOW,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
        })
    }

    /// Worst-case time from [`Sidecar::start`] to a usable server: every allowed
    /// attempt plus the backoffs between them, plus slack.
    ///
    /// The slack matters. A server that binds its port and then never finishes
    /// loading is killed by [`run_attempt`] once `startup_timeout` elapses, and
    /// after `max_crashes` such attempts the supervisor reports
    /// [`SidecarState::Suspect`] — a verdict that names the model and carries
    /// the server's own output. Without slack, [`Sidecar::start`]'s deadline
    /// would expire at the same instant, and the caller would race between that
    /// diagnosis and a bare [`Error::StartupTimeout`]. The budget is the
    /// backstop for a wedged *supervisor*, not for a wedged server.
    fn startup_budget(&self) -> Duration {
        let attempts = self.max_crashes.max(1);
        let spawning = self.startup_timeout.saturating_mul(attempts);
        let waiting: Duration = (1..attempts).map(|attempt| self.backoff(attempt)).sum();
        spawning + waiting + SUPERVISOR_SLACK
    }

    /// Exponential backoff before the `attempt`-th restart (1-based).
    fn backoff(&self, attempt: u32) -> Duration {
        let doubling = 2u32.saturating_pow(attempt.saturating_sub(1));
        match self.initial_backoff.checked_mul(doubling) {
            Some(backoff) => backoff.min(self.max_backoff),
            None => self.max_backoff,
        }
    }

    /// The command line, in order.
    fn args(&self) -> Vec<String> {
        let mut args = vec![
            "--model".into(),
            self.model_path.display().to_string(),
            "--alias".into(),
            self.model_id.to_string(),
            "--host".into(),
            "127.0.0.1".into(),
            "--port".into(),
            self.port.to_string(),
            "--n-gpu-layers".into(),
            self.n_gpu_layers.to_string(),
            "--ctx-size".into(),
            self.ctx_size.to_string(),
            "--api-key".into(),
            self.api_key.clone(),
            // Use the model's own chat template. Without it llama-server falls
            // back to a generic one and the model emits tool calls as prose,
            // which the agent loop cannot parse.
            "--jinja".into(),
        ];
        args.extend(self.extra_args.iter().cloned());
        args
    }
}

/// A running, supervised `llama-server`.
///
/// Dropping the sidecar stops the supervisor and kills the child.
pub struct Sidecar {
    port: u16,
    api_key: String,
    model_id: ModelId,
    max_crashes: u32,
    state: watch::Receiver<SidecarState>,
    shutdown: watch::Sender<bool>,
    supervisor: Option<JoinHandle<()>>,
    output: OutputTail,
}

impl Sidecar {
    /// Launch `llama-server` and wait until it answers `/health`.
    ///
    /// Returns [`Error::ModelSuspect`] when the server fails
    /// [`SidecarConfig::max_crashes`] times in a row, and
    /// [`Error::StartupTimeout`] when it never gets there in time. In both cases
    /// the child has been killed before the error is returned.
    pub async fn start(config: SidecarConfig) -> Result<Self> {
        let budget = config.startup_budget();
        let (state_tx, state_rx) = watch::channel(SidecarState::Starting);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let output = OutputTail::new();

        let supervisor = tokio::spawn(supervise(
            config.clone(),
            state_tx,
            shutdown_rx,
            output.clone(),
        ));

        let mut sidecar = Self {
            port: config.port,
            api_key: config.api_key,
            model_id: config.model_id,
            max_crashes: config.max_crashes,
            state: state_rx,
            shutdown: shutdown_tx,
            supervisor: Some(supervisor),
            output,
        };

        match sidecar.wait_until_ready(budget).await {
            Ok(()) => Ok(sidecar),
            Err(error) => {
                sidecar.stop().await;
                Err(error)
            }
        }
    }

    /// Block until the server is ready, or until it gives up.
    async fn wait_until_ready(&mut self, budget: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            // Scoped so the borrow guard is dropped before the await below.
            let current = self.state.borrow_and_update().clone();
            match current {
                SidecarState::Ready => return Ok(()),
                SidecarState::Suspect { reason } => {
                    return Err(Error::ModelSuspect {
                        model: self.model_id.to_string(),
                        crashes: self.max_crashes,
                        last_output: Some(reason),
                    });
                }
                SidecarState::Stopped => return Err(Error::NotRunning),
                _ => {}
            }
            if tokio::time::timeout_at(deadline, self.state.changed())
                .await
                .is_err()
            {
                return Err(Error::StartupTimeout(budget));
            }
        }
    }

    /// The OpenAI-compatible base URL, including the `/v1` suffix IronClaw's
    /// `openai_compatible` provider expects.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    /// The loopback port `llama-server` listens on, stable across restarts.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The unauthenticated liveness endpoint.
    pub fn health_url(&self) -> String {
        format!("http://127.0.0.1:{}/health", self.port)
    }

    /// The bearer token callers must present.
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// The model this sidecar is serving.
    pub fn model_id(&self) -> &ModelId {
        &self.model_id
    }

    /// The current state.
    pub fn state(&self) -> SidecarState {
        self.state.borrow().clone()
    }

    /// Watch for state transitions, for a health badge in the UI.
    pub fn subscribe(&self) -> watch::Receiver<SidecarState> {
        self.state.clone()
    }

    /// The last few hundred lines the server printed, for a diagnostics pane.
    pub fn output_tail(&self) -> String {
        self.output.snapshot()
    }

    /// Stop the server and wait for the supervisor to wind down.
    pub async fn stop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(handle) = self.supervisor.take() {
            // The supervisor kills the child before returning. If it does not
            // wind down promptly, aborting it drops the `Child`, and
            // `kill_on_drop` finishes the job.
            if tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .is_err()
            {
                tracing::warn!("llama-server supervisor did not stop in time; aborting it");
            }
        }
    }
}

impl std::fmt::Debug for Sidecar {
    /// Redacts the API key, which would otherwise reach any log line that
    /// formats the sidecar.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sidecar")
            .field("model_id", &self.model_id)
            .field("port", &self.port)
            .field("state", &self.state())
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(handle) = self.supervisor.take() {
            // Dropping the supervisor's stack drops the `Child`, whose
            // `kill_on_drop` terminates `llama-server`. This is what keeps a
            // panicking or short-circuiting caller from orphaning the process.
            handle.abort();
        }
    }
}

/// Outcome of watching one `llama-server` process.
enum Attempt {
    /// The caller asked us to stop.
    Shutdown,
    /// The process is gone, one way or another.
    Failed {
        /// How it died, for the log.
        detail: String,
        /// When it first answered `200 OK`, if it ever did.
        ready_since: Option<Instant>,
    },
}

/// Keep one `llama-server` alive until told to stop or until the model is
/// declared suspect.
async fn supervise(
    config: SidecarConfig,
    state: watch::Sender<SidecarState>,
    mut shutdown: watch::Receiver<bool>,
    output: OutputTail,
) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            let _ = state.send(SidecarState::Suspect {
                reason: format!("could not build the health-check client: {error}"),
            });
            return;
        }
    };

    let mut consecutive_failures = 0u32;
    loop {
        if *shutdown.borrow() {
            let _ = state.send(SidecarState::Stopped);
            return;
        }

        let _ = state.send(SidecarState::Starting);
        let attempt = match spawn(&config, &output) {
            Ok(child) => run_attempt(child, &config, &client, &state, &mut shutdown).await,
            // A spawn failure (missing binary, no permission) is not going to
            // fix itself on a retry, but it costs one restart to find out and
            // keeps the failure path uniform.
            Err(error) => Attempt::Failed {
                detail: error.to_string(),
                ready_since: None,
            },
        };

        let (detail, ready_since) = match attempt {
            Attempt::Shutdown => {
                let _ = state.send(SidecarState::Stopped);
                return;
            }
            Attempt::Failed {
                detail,
                ready_since,
            } => (detail, ready_since),
        };

        // A server that held up its end for long enough earns a clean slate.
        let was_stable = ready_since
            .map(|since| since.elapsed() >= config.stability_window)
            .unwrap_or(false);
        if was_stable {
            consecutive_failures = 0;
        }
        consecutive_failures += 1;

        tracing::warn!(
            model = %config.model_id,
            attempt = consecutive_failures,
            %detail,
            "llama-server exited"
        );

        if consecutive_failures >= config.max_crashes {
            let _ = state.send(SidecarState::Suspect {
                reason: suspect_reason(&config, consecutive_failures, &detail, &output),
            });
            return;
        }

        let backoff = config.backoff(consecutive_failures);
        let _ = state.send(SidecarState::Restarting {
            attempt: consecutive_failures,
            backoff_ms: backoff.as_millis().min(u128::from(u64::MAX)) as u64,
        });
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown.changed() => {
                let _ = state.send(SidecarState::Stopped);
                return;
            }
        }
    }
}

/// Watch one child until it dies, wedges, or we're told to stop.
async fn run_attempt(
    mut child: Child,
    config: &SidecarConfig,
    client: &reqwest::Client,
    state: &watch::Sender<SidecarState>,
    shutdown: &mut watch::Receiver<bool>,
) -> Attempt {
    let health_url = format!("http://127.0.0.1:{}/health", config.port);
    let spawned_at = Instant::now();
    let mut ready_since: Option<Instant> = None;

    loop {
        if *shutdown.borrow_and_update() {
            kill(&mut child).await;
            return Attempt::Shutdown;
        }

        // `try_wait` rather than racing `wait()` in a `select!`: reaping is not
        // cancel-safe in every tokio version, and a quarter-second of detection
        // latency on a process that just died costs nothing.
        match child.try_wait() {
            Ok(Some(status)) => {
                return Attempt::Failed {
                    detail: format!("process exited with {status}"),
                    ready_since,
                };
            }
            Ok(None) => {}
            Err(error) => {
                kill(&mut child).await;
                return Attempt::Failed {
                    detail: format!("could not check on the process: {error}"),
                    ready_since,
                };
            }
        }

        match probe_health(client, &health_url).await {
            Health::Ready => {
                if ready_since.is_none() {
                    ready_since = Some(Instant::now());
                    tracing::info!(
                        model = %config.model_id,
                        port = config.port,
                        load_time = ?spawned_at.elapsed(),
                        "llama-server is ready"
                    );
                }
                let _ = state.send_if_modified(|current| {
                    if current.is_ready() {
                        false
                    } else {
                        *current = SidecarState::Ready;
                        true
                    }
                });
            }
            Health::Loading => {
                let _ = state.send_if_modified(|current| {
                    if matches!(current, SidecarState::Loading) {
                        false
                    } else {
                        *current = SidecarState::Loading;
                        true
                    }
                });
            }
            Health::Unreachable => {}
        }

        // Up but never ready: wedged on a model it cannot load. Kill it so the
        // failure is counted rather than waited on forever.
        if ready_since.is_none() && spawned_at.elapsed() >= config.startup_timeout {
            kill(&mut child).await;
            return Attempt::Failed {
                detail: format!("did not become healthy within {:?}", config.startup_timeout),
                ready_since,
            };
        }

        tokio::select! {
            _ = tokio::time::sleep(HEALTH_POLL_INTERVAL) => {}
            _ = shutdown.changed() => {
                kill(&mut child).await;
                return Attempt::Shutdown;
            }
        }
    }
}

/// What `/health` said.
enum Health {
    /// `200 OK`.
    Ready,
    /// `503 Loading model`.
    Loading,
    /// Not listening yet, or not answering.
    Unreachable,
}

async fn probe_health(client: &reqwest::Client, url: &str) -> Health {
    match client.get(url).send().await {
        Ok(response) if response.status().is_success() => Health::Ready,
        // `llama-server` binds the port before the weights are resident and
        // answers 503 until they are.
        Ok(_) => Health::Loading,
        Err(_) => Health::Unreachable,
    }
}

fn spawn(config: &SidecarConfig, output: &OutputTail) -> Result<Child> {
    let mut command = Command::new(&config.server_bin);
    command
        .args(config.args())
        .envs(config.env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The last line of defence against an orphaned `llama-server` holding a
        // GPU allocation after the widget goes away.
        .kill_on_drop(true);

    let mut child = command.spawn().map_err(|source| {
        Error::io(format!("launching {}", config.server_bin.display()), source)
    })?;

    // Drain both pipes. An unread pipe fills and blocks the child, and
    // llama.cpp is chatty enough to hit that during model load.
    if let Some(stdout) = child.stdout.take() {
        output.drain(stdout);
    }
    if let Some(stderr) = child.stderr.take() {
        output.drain(stderr);
    }
    Ok(child)
}

async fn kill(child: &mut Child) {
    if let Err(error) = child.kill().await {
        tracing::warn!(%error, "could not kill llama-server");
    }
}

fn suspect_reason(
    config: &SidecarConfig,
    crashes: u32,
    detail: &str,
    output: &OutputTail,
) -> String {
    let tail = output.snapshot();
    let tail = tail.trim();
    let mut reason = format!(
        "llama-server failed {crashes} times in a row loading {} ({detail}). \
         The model will not be retried automatically; try a smaller context, \
         fewer GPU layers than {}, or a different model.",
        config.model_path.display(),
        config.n_gpu_layers,
    );
    if !tail.is_empty() {
        reason.push_str("\n--- llama-server output ---\n");
        reason.push_str(tail);
    }
    reason
}

/// A bounded, shared ring of the child's most recent output lines.
#[derive(Clone)]
struct OutputTail {
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl OutputTail {
    fn new() -> Self {
        Self {
            lines: Arc::new(Mutex::new(VecDeque::with_capacity(OUTPUT_TAIL_LINES))),
        }
    }

    /// Read `pipe` to EOF in the background, keeping the last
    /// [`OUTPUT_TAIL_LINES`] lines.
    fn drain<R>(&self, pipe: R)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        let lines = Arc::clone(&self.lines);
        tokio::spawn(async move {
            let mut reader = BufReader::new(pipe).lines();
            // A read error means the child is gone; the supervisor notices via
            // `try_wait` and there is nothing to recover here.
            while let Ok(Some(line)) = reader.next_line().await {
                let Ok(mut guard) = lines.lock() else {
                    return; // silent-ok: a poisoned tail buffer only costs diagnostics
                };
                if guard.len() == OUTPUT_TAIL_LINES {
                    guard.pop_front();
                }
                guard.push_back(line);
            }
        });
    }

    fn snapshot(&self) -> String {
        match self.lines.lock() {
            Ok(guard) => guard.iter().cloned().collect::<Vec<_>>().join("\n"),
            Err(_) => String::new(), // silent-ok: diagnostics only
        }
    }
}

/// Reserve a loopback port by binding `:0` and reading back the assignment.
///
/// The listener closes immediately, leaving a race window before `llama-server`
/// binds it. There is no way to avoid this: `llama-server` does not report the
/// port it chose when given `--port 0`, so the port must be decided up front.
fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|source| Error::io("reserving a loopback port for llama-server", source))?;
    let port = listener
        .local_addr()
        .map_err(|source| Error::io("reading back the reserved port", source))?
        .port();
    Ok(port)
}

/// 128 bits of randomness, which is plenty for a token that lives as long as one
/// process and never leaves the machine.
fn random_api_key() -> String {
    format!(
        "ic-{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SidecarConfig {
        SidecarConfig::new(
            PathBuf::from("llama-server"),
            PathBuf::from("model.gguf"),
            ModelId::new("test-model").expect("valid"),
        )
        .expect("a free port")
    }

    #[test]
    fn every_launch_reuses_the_reserved_port() {
        let config = config();
        let first = config.args();
        let second = config.args();
        assert_eq!(first, second);
        let port_index = first
            .iter()
            .position(|arg| arg == "--port")
            .expect("--port");
        assert_eq!(first[port_index + 1], config.port.to_string());
    }

    #[test]
    fn args_carry_the_placement_and_the_chat_template() {
        let mut config = config();
        config.n_gpu_layers = 33;
        config.ctx_size = 8192;
        let args = config.args();
        let value_of = |flag: &str| {
            let index = args.iter().position(|arg| arg == flag)?;
            args.get(index + 1).cloned()
        };
        assert_eq!(value_of("--n-gpu-layers").as_deref(), Some("33"));
        assert_eq!(value_of("--ctx-size").as_deref(), Some("8192"));
        assert_eq!(value_of("--alias").as_deref(), Some("test-model"));
        assert_eq!(value_of("--host").as_deref(), Some("127.0.0.1"));
        // Tool calls depend on the model's own template.
        assert!(args.iter().any(|arg| arg == "--jinja"));
    }

    #[test]
    fn extra_args_are_appended_after_ours() {
        let mut config = config();
        config.extra_args = vec!["--threads".into(), "4".into()];
        let args = config.args();
        assert_eq!(&args[args.len() - 2..], &["--threads", "4"]);
    }

    #[test]
    fn api_keys_differ_between_configs() {
        assert_ne!(config().api_key, config().api_key);
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        let mut config = config();
        config.initial_backoff = Duration::from_secs(1);
        config.max_backoff = Duration::from_secs(4);
        assert_eq!(config.backoff(1), Duration::from_secs(1));
        assert_eq!(config.backoff(2), Duration::from_secs(2));
        assert_eq!(config.backoff(3), Duration::from_secs(4));
        // Capped, and no overflow at absurd attempt counts.
        assert_eq!(config.backoff(64), Duration::from_secs(4));
        assert_eq!(config.backoff(u32::MAX), Duration::from_secs(4));
    }

    #[test]
    fn the_startup_budget_outlasts_the_supervisors_own_verdict() {
        let mut config = config();
        config.startup_timeout = Duration::from_secs(10);
        config.initial_backoff = Duration::from_secs(1);
        config.max_crashes = 2;
        // Two attempts of 10s and one 1s backoff between them is the longest the
        // supervisor can take to declare the model suspect; the budget must
        // exceed that so the caller sees the diagnosis, not a timeout.
        let supervisor_worst_case = Duration::from_secs(21);
        assert!(config.startup_budget() > supervisor_worst_case);
        assert_eq!(
            config.startup_budget(),
            supervisor_worst_case + SUPERVISOR_SLACK
        );
    }

    #[test]
    fn state_terminality() {
        assert!(SidecarState::Stopped.is_terminal());
        assert!(
            SidecarState::Suspect {
                reason: String::new()
            }
            .is_terminal()
        );
        assert!(!SidecarState::Loading.is_terminal());
        assert!(SidecarState::Ready.is_ready());
    }

    #[test]
    fn a_reserved_port_is_bindable() {
        let port = free_port().expect("a port");
        assert_ne!(port, 0);
        // The listener is closed again, so the port must be re-bindable — that
        // is exactly what `llama-server` is about to do with it.
        std::net::TcpListener::bind(("127.0.0.1", port)).expect("port should be free again");
    }
}
