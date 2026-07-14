//! Keeping `ironclaw-reborn serve` alive.
//!
//! The widget is the only user-facing process; the gateway is a child it owns.
//! Four things make this different from supervising `llama-server`
//! ([`ic_llama::server`]):
//!
//! **Readiness is an authenticated read.** `serve` has no `/health` route
//! ([`crate::gateway_client`] uses `GET /threads`), so a successful health check
//! proves three things at once: the listener is bound, the runtime booted, and
//! the token we minted is the token it accepted.
//!
//! **A `401` is fatal, not a retry.** If the gateway is up but rejects our
//! token, restarting it changes nothing — the credential store and the child's
//! environment disagree, and only the user can fix that (by clearing the token).
//! Retrying would loop forever against a process that is working perfectly.
//!
//! **The port is chosen by us, once.** `serve --port 0` binds an ephemeral port
//! and never reports it (`serve.rs:467`), so the supervisor picks a free port
//! and passes it explicitly, reusing it across restarts.
//!
//! **Children die with us.** Every spawn is assigned to a [`ProcessJob`], so a
//! hard-killed widget does not leave a gateway holding the libSQL write lock.

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
use crate::gateway_client::GatewayClient;
use crate::job_object::ProcessJob;

/// How often readiness is polled while starting, and liveness once running.
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Lines of gateway output kept for the diagnostics pane.
const OUTPUT_TAIL_LINES: usize = 300;

/// Healthy for this long and the crash counter resets.
const DEFAULT_STABILITY_WINDOW: Duration = Duration::from_secs(60);

/// So the supervisor's own verdict beats the caller's deadline. See
/// [`GatewayConfig::startup_budget`].
const SUPERVISOR_SLACK: Duration = Duration::from_secs(5);

/// The runtime's default owner. `IRONCLAW_REBORN_WEBUI_USER_ID` must equal
/// `[identity].default_owner`, or threads the widget creates are invisible to
/// the turn runner — a silent failure discovered in the Phase 0 smoke.
pub const DEFAULT_OWNER_USER_ID: &str = "reborn-cli";

/// The libSQL profile. Not `local-dev-yolo`: that grants trusted-laptop host
/// access and must be an explicit user decision, not a default.
pub const DEFAULT_PROFILE: &str = "local-dev";

/// What the gateway is doing. Drives the widget's health badge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum GatewayState {
    /// Spawned, not answering yet.
    Starting,
    /// Answering authenticated reads.
    Ready,
    /// Died; will be respawned.
    Restarting {
        /// Which consecutive failure this is.
        attempt: u32,
        /// How long until the next spawn.
        backoff_ms: u64,
    },
    /// Terminal. Either it failed too many times, or it is up and rejecting our
    /// token.
    Unhealthy {
        /// User-facing explanation.
        reason: String,
    },
    /// Stopped at our request. Terminal.
    Stopped,
}

impl GatewayState {
    /// Whether the gateway can serve requests.
    pub fn is_ready(&self) -> bool {
        matches!(self, GatewayState::Ready)
    }

    /// Whether the supervisor has given up.
    pub fn is_terminal(&self) -> bool {
        matches!(self, GatewayState::Unhealthy { .. } | GatewayState::Stopped)
    }
}

/// How to launch `ironclaw-reborn serve`.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// Path to the `ironclaw-reborn` binary.
    pub binary: PathBuf,
    /// `IRONCLAW_REBORN_HOME` — where the libSQL database and workspace live.
    pub home: PathBuf,
    /// `IRONCLAW_REBORN_PROFILE`.
    pub profile: String,
    /// The loopback port, held stable across restarts.
    pub port: u16,
    /// The bearer token, from the OS credential store.
    pub token: String,
    /// `IRONCLAW_REBORN_WEBUI_USER_ID`.
    pub user_id: String,
    /// Provider environment, e.g. what `ic_llama::LlmEnv` emits for a local
    /// model. Applied on top of the inherited environment.
    pub llm_env: Vec<(String, String)>,
    /// Runtime switches the widget decides per launch — today, whether the trigger
    /// poller runs (`IRONCLAW_TRIGGER_POLLER_ENABLED`, which the runtime leaves
    /// **off** by default, so a scheduled automation never fires without it).
    ///
    /// Separate from [`Self::llm_env`] because it is not a provider choice and is
    /// not rebuilt when the provider changes.
    pub extra_env: Vec<(String, String)>,
    /// How long one spawn has to become ready.
    pub startup_timeout: Duration,
    /// Consecutive failures before giving up.
    pub max_crashes: u32,
    /// Healthy for this long resets the failure count.
    pub stability_window: Duration,
    /// Delay before the first restart; doubles thereafter.
    pub initial_backoff: Duration,
    /// Ceiling on the restart delay.
    pub max_backoff: Duration,
}

impl GatewayConfig {
    /// A config with the desktop defaults and a freshly reserved loopback port.
    pub fn new(binary: PathBuf, home: PathBuf, token: String) -> Result<Self> {
        Ok(Self {
            binary,
            home,
            profile: DEFAULT_PROFILE.to_string(),
            port: free_port()?,
            token,
            user_id: DEFAULT_OWNER_USER_ID.to_string(),
            llm_env: Vec::new(),
            extra_env: Vec::new(),
            // The first boot installs bundled skills and runs migrations.
            startup_timeout: Duration::from_secs(90),
            max_crashes: 3,
            stability_window: DEFAULT_STABILITY_WINDOW,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
        })
    }

    /// The base URL the gateway will serve on.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Worst case from `start` to ready, plus slack so the supervisor's own
    /// diagnosis (which names the failure) wins the race against the caller's
    /// bare timeout.
    fn startup_budget(&self) -> Duration {
        let attempts = self.max_crashes.max(1);
        let spawning = self.startup_timeout.saturating_mul(attempts);
        let waiting: Duration = (1..attempts).map(|attempt| self.backoff(attempt)).sum();
        spawning + waiting + SUPERVISOR_SLACK
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let doubling = 2u32.saturating_pow(attempt.saturating_sub(1));
        match self.initial_backoff.checked_mul(doubling) {
            Some(backoff) => backoff.min(self.max_backoff),
            None => self.max_backoff,
        }
    }

    fn args(&self) -> Vec<String> {
        vec![
            "serve".into(),
            "--host".into(),
            "127.0.0.1".into(),
            "--port".into(),
            self.port.to_string(),
        ]
    }

    /// The environment the child needs, on top of the inherited one.
    fn env(&self) -> Vec<(String, String)> {
        let mut env = vec![
            (
                "IRONCLAW_REBORN_HOME".into(),
                self.home.display().to_string(),
            ),
            ("IRONCLAW_REBORN_PROFILE".into(), self.profile.clone()),
            ("IRONCLAW_REBORN_WEBUI_TOKEN".into(), self.token.clone()),
            ("IRONCLAW_REBORN_WEBUI_USER_ID".into(), self.user_id.clone()),
        ];
        env.extend(self.llm_env.iter().cloned());
        env.extend(self.extra_env.iter().cloned());
        env
    }
}

/// A supervised `ironclaw-reborn serve`.
///
/// Dropping it stops the supervisor; the [`ProcessJob`] kills the child.
pub struct GatewaySupervisor {
    client: GatewayClient,
    state: watch::Receiver<GatewayState>,
    shutdown: watch::Sender<bool>,
    supervisor: Option<JoinHandle<()>>,
    output: OutputTail,
}

impl GatewaySupervisor {
    /// Launch the gateway and wait until it answers an authenticated read.
    ///
    /// `job` owns the process tree; pass the same job used for every other child
    /// so one handle close takes them all down.
    pub async fn start(config: GatewayConfig, job: Arc<ProcessJob>) -> Result<Self> {
        let client = GatewayClient::new(config.base_url(), &config.token)?;
        let budget = config.startup_budget();
        let (state_tx, state_rx) = watch::channel(GatewayState::Starting);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let output = OutputTail::new();

        let supervisor = tokio::spawn(supervise(
            config,
            job,
            client.clone(),
            state_tx,
            shutdown_rx,
            output.clone(),
        ));

        let mut gateway = Self {
            client,
            state: state_rx,
            shutdown: shutdown_tx,
            supervisor: Some(supervisor),
            output,
        };
        match gateway.wait_until_ready(budget).await {
            Ok(()) => Ok(gateway),
            Err(error) => {
                gateway.stop().await;
                Err(error)
            }
        }
    }

    async fn wait_until_ready(&mut self, budget: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            match self.state.borrow_and_update().clone() {
                GatewayState::Ready => return Ok(()),
                GatewayState::Unhealthy { reason } => {
                    return Err(Error::GatewayUnhealthy {
                        crashes: 0,
                        last_output: Some(reason),
                    });
                }
                GatewayState::Stopped => {
                    return Err(Error::GatewayUnhealthy {
                        crashes: 0,
                        last_output: Some("the gateway stopped during startup".into()),
                    });
                }
                _ => {}
            }
            if tokio::time::timeout_at(deadline, self.state.changed())
                .await
                .is_err()
            {
                return Err(Error::GatewayStartupTimeout(budget));
            }
        }
    }

    /// A client bound to this gateway.
    pub fn client(&self) -> &GatewayClient {
        &self.client
    }

    /// The current state.
    pub fn state(&self) -> GatewayState {
        self.state.borrow().clone()
    }

    /// Watch for state transitions, for the widget's health badge.
    pub fn subscribe(&self) -> watch::Receiver<GatewayState> {
        self.state.clone()
    }

    /// The last few hundred lines the gateway printed.
    pub fn output_tail(&self) -> String {
        self.output.snapshot()
    }

    /// Re-check health now, rather than waiting for the next poll.
    ///
    /// Call this on `WM_POWERBROADCAST` resume: a laptop that slept for an hour
    /// has a gateway whose sockets and database handles may or may not have
    /// survived, and the user is looking at the widget right now.
    pub async fn recheck(&self) -> Result<()> {
        self.client.health().await
    }

    /// Stop the gateway and wait for the supervisor to wind down.
    pub async fn stop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(handle) = self.supervisor.take()
            && tokio::time::timeout(Duration::from_secs(10), handle)
                .await
                .is_err()
        {
            tracing::warn!("the gateway supervisor did not stop in time");
        }
    }
}

impl std::fmt::Debug for GatewaySupervisor {
    /// The client redacts the bearer token; nothing else here is a secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewaySupervisor")
            .field("client", &self.client)
            .field("state", &self.state())
            .finish()
    }
}

impl Drop for GatewaySupervisor {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(handle) = self.supervisor.take() {
            handle.abort();
        }
    }
}

/// Outcome of watching one gateway process.
enum Attempt {
    Shutdown,
    /// The gateway is up but will never accept our token.
    TokenRejected,
    Failed {
        detail: String,
        ready_since: Option<Instant>,
    },
}

async fn supervise(
    config: GatewayConfig,
    job: Arc<ProcessJob>,
    client: GatewayClient,
    state: watch::Sender<GatewayState>,
    mut shutdown: watch::Receiver<bool>,
    output: OutputTail,
) {
    let mut consecutive_failures = 0u32;
    loop {
        if *shutdown.borrow() {
            let _ = state.send(GatewayState::Stopped);
            return;
        }
        let _ = state.send(GatewayState::Starting);

        let attempt = match spawn(&config, &job, &output) {
            Ok(child) => run_attempt(child, &config, &client, &state, &mut shutdown).await,
            Err(error) => Attempt::Failed {
                detail: error.to_string(),
                ready_since: None,
            },
        };

        let (detail, ready_since) = match attempt {
            Attempt::Shutdown => {
                let _ = state.send(GatewayState::Stopped);
                return;
            }
            // Restarting cannot make the gateway accept a token it does not
            // have. Stop, and let the user clear the credential.
            Attempt::TokenRejected => {
                let _ = state.send(GatewayState::Unhealthy {
                    reason: "the gateway is running but rejected our bearer token. \
                             Clear the saved token and restart the app."
                        .into(),
                });
                return;
            }
            Attempt::Failed {
                detail,
                ready_since,
            } => (detail, ready_since),
        };

        if ready_since.is_some_and(|since| since.elapsed() >= config.stability_window) {
            consecutive_failures = 0;
        }
        consecutive_failures += 1;
        tracing::warn!(attempt = consecutive_failures, %detail, "ironclaw-reborn exited");

        if consecutive_failures >= config.max_crashes {
            let _ = state.send(GatewayState::Unhealthy {
                reason: unhealthy_reason(consecutive_failures, &detail, &output),
            });
            return;
        }

        let backoff = config.backoff(consecutive_failures);
        let _ = state.send(GatewayState::Restarting {
            attempt: consecutive_failures,
            backoff_ms: backoff.as_millis().min(u128::from(u64::MAX)) as u64,
        });
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown.changed() => {
                let _ = state.send(GatewayState::Stopped);
                return;
            }
        }
    }
}

async fn run_attempt(
    mut child: Child,
    config: &GatewayConfig,
    client: &GatewayClient,
    state: &watch::Sender<GatewayState>,
    shutdown: &mut watch::Receiver<bool>,
) -> Attempt {
    let spawned_at = Instant::now();
    let mut ready_since: Option<Instant> = None;

    loop {
        if *shutdown.borrow_and_update() {
            kill(&mut child).await;
            return Attempt::Shutdown;
        }

        // `try_wait` rather than racing `wait()` in a `select!`: reaping is not
        // cancel-safe in every tokio version, and half a second of detection
        // latency on a dead process costs nothing.
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

        match client.health().await {
            Ok(()) => {
                if ready_since.is_none() {
                    ready_since = Some(Instant::now());
                    tracing::info!(
                        port = config.port,
                        boot_time = ?spawned_at.elapsed(),
                        "ironclaw-reborn is ready"
                    );
                }
                let _ = state.send_if_modified(|current| {
                    if current.is_ready() {
                        false
                    } else {
                        *current = GatewayState::Ready;
                        true
                    }
                });
            }
            // The process is answering, so it is not broken — it simply does not
            // believe our token. No number of restarts fixes that.
            Err(error) if error.is_unauthorized() => {
                kill(&mut child).await;
                return Attempt::TokenRejected;
            }
            // Not listening yet, or the runtime is still booting.
            Err(_) => {}
        }

        if ready_since.is_none() && spawned_at.elapsed() >= config.startup_timeout {
            kill(&mut child).await;
            return Attempt::Failed {
                detail: format!("did not become ready within {:?}", config.startup_timeout),
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

fn spawn(config: &GatewayConfig, job: &ProcessJob, output: &OutputTail) -> Result<Child> {
    let mut command = Command::new(&config.binary);
    command
        .args(config.args())
        .envs(config.env())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|source| Error::io(format!("launching {}", config.binary.display()), source))?;

    // Contain the child before it can spawn anything of its own.
    if let Err(error) = job.assign(&child) {
        // A child outside the job would survive a hard kill of the widget and
        // hold the libSQL write lock. Better to fail the launch.
        let _ = child.start_kill();
        return Err(error);
    }

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
        tracing::warn!(%error, "could not kill ironclaw-reborn");
    }
}

fn unhealthy_reason(crashes: u32, detail: &str, output: &OutputTail) -> String {
    let tail = output.snapshot();
    let tail = tail.trim();
    let mut reason = format!(
        "ironclaw-reborn failed {crashes} times in a row ({detail}). It will not be restarted automatically."
    );
    if !tail.is_empty() {
        reason.push_str("\n--- gateway output ---\n");
        reason.push_str(tail);
    }
    reason
}

/// A bounded, shared ring of the gateway's most recent output lines.
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

    fn drain<R>(&self, pipe: R)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        let lines = Arc::clone(&self.lines);
        tokio::spawn(async move {
            let mut reader = BufReader::new(pipe).lines();
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

/// Reserve a loopback port by binding `:0` and reading it back.
///
/// `serve --port 0` binds an ephemeral port but never reports it
/// (`bound_addr_tx: None`, `serve.rs:467`), so the port must be decided here.
/// There is a small race before the child rebinds; it is unavoidable.
fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|source| Error::io("reserving a loopback port for the gateway", source))?;
    let port = listener
        .local_addr()
        .map_err(|source| Error::io("reading back the reserved port", source))?
        .port();
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> GatewayConfig {
        GatewayConfig::new(
            PathBuf::from("ironclaw-reborn"),
            PathBuf::from("/tmp/home"),
            "token".into(),
        )
        .expect("a free port")
    }

    #[test]
    fn the_serve_command_pins_an_explicit_loopback_port() {
        let config = config();
        let args = config.args();
        assert_eq!(args[0], "serve");
        let port = args
            .iter()
            .position(|arg| arg == "--port")
            .and_then(|index| args.get(index + 1))
            .expect("--port");
        // `--port 0` would bind an ephemeral port the gateway never reports.
        assert_ne!(port, "0");
        assert_eq!(port, &config.port.to_string());
        assert!(args.contains(&"127.0.0.1".to_string()));
    }

    #[test]
    fn the_environment_carries_the_token_the_profile_and_the_owner() {
        let config = config();
        let env: std::collections::HashMap<_, _> = config.env().into_iter().collect();
        assert_eq!(env["IRONCLAW_REBORN_WEBUI_TOKEN"], "token");
        assert_eq!(env["IRONCLAW_REBORN_PROFILE"], "local-dev");
        // Must equal `[identity].default_owner`, or WebUI-created threads are
        // invisible to the turn runner.
        assert_eq!(env["IRONCLAW_REBORN_WEBUI_USER_ID"], DEFAULT_OWNER_USER_ID);
    }

    #[test]
    fn the_default_profile_is_not_the_host_access_one() {
        // `local-dev-yolo` grants trusted-laptop host shell/FS access. It must be
        // an explicit user decision, never a default.
        assert_eq!(config().profile, "local-dev");
        assert_ne!(config().profile, "local-dev-yolo");
    }

    #[test]
    fn provider_environment_is_layered_on_top() {
        let mut config = config();
        config.llm_env = vec![
            ("LLM_BACKEND".into(), "openai_compatible".into()),
            ("LLM_BASE_URL".into(), "http://127.0.0.1:9/v1".into()),
        ];
        let env: std::collections::HashMap<_, _> = config.env().into_iter().collect();
        assert_eq!(env["LLM_BACKEND"], "openai_compatible");
        assert_eq!(env["IRONCLAW_REBORN_PROFILE"], "local-dev");
    }

    #[test]
    fn the_trigger_poller_is_only_on_when_the_widget_asks_for_it() {
        // The runtime's own default is *off*, so a scheduled automation never fires
        // unless this variable is in the child's environment. Ambient mode is the
        // only thing that puts it there — see `ambient`.
        let plain: std::collections::HashMap<_, _> = config().env().into_iter().collect();
        assert!(!plain.contains_key("IRONCLAW_TRIGGER_POLLER_ENABLED"));

        let mut ambient = config();
        ambient.extra_env = vec![("IRONCLAW_TRIGGER_POLLER_ENABLED".into(), "true".into())];
        let env: std::collections::HashMap<_, _> = ambient.env().into_iter().collect();
        assert_eq!(env["IRONCLAW_TRIGGER_POLLER_ENABLED"], "true");
    }

    #[test]
    fn backoff_doubles_and_is_capped_without_overflowing() {
        let mut config = config();
        config.initial_backoff = Duration::from_secs(1);
        config.max_backoff = Duration::from_secs(4);
        assert_eq!(config.backoff(1), Duration::from_secs(1));
        assert_eq!(config.backoff(2), Duration::from_secs(2));
        assert_eq!(config.backoff(3), Duration::from_secs(4));
        assert_eq!(config.backoff(u32::MAX), Duration::from_secs(4));
    }

    #[test]
    fn the_startup_budget_outlasts_the_supervisors_own_verdict() {
        let mut config = config();
        config.startup_timeout = Duration::from_secs(10);
        config.initial_backoff = Duration::from_secs(1);
        config.max_crashes = 2;
        assert_eq!(
            config.startup_budget(),
            Duration::from_secs(21) + SUPERVISOR_SLACK
        );
    }

    #[test]
    fn state_terminality() {
        assert!(GatewayState::Stopped.is_terminal());
        assert!(
            GatewayState::Unhealthy {
                reason: String::new()
            }
            .is_terminal()
        );
        assert!(!GatewayState::Starting.is_terminal());
        assert!(GatewayState::Ready.is_ready());
    }

    #[test]
    fn the_base_url_matches_the_reserved_port() {
        let config = config();
        assert_eq!(
            config.base_url(),
            format!("http://127.0.0.1:{}", config.port)
        );
    }
}
