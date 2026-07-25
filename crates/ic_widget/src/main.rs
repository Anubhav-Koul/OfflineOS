//! The Tauri shell: windows, tray, hotkey, and the bridge to the UI.
//!
//! Everything with logic worth testing lives in the library (`src/lib.rs`), which
//! builds without Tauri. This file is the wiring:
//!
//! - one [`ProcessJob`] owns every child, so a hard-killed widget orphans nothing;
//! - the gateway starts in the background and the widget shows a health badge
//!   rather than a splash screen, because a first boot runs migrations and
//!   installs bundled skills;
//! - a per-thread SSE pump turns gateway events into `chat://event` for the UI;
//! - the widget's position is remembered per monitor arrangement.
//!
//! The UI never sees the bearer token, the gateway's port, or a raw HTTP status.

// Release builds must not pop a console window behind the widget.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ic_llama::download::{Downloader, Progress, ProgressFn};
use ic_llama::{
    CloudFallback, Digest, HubModel, LocalLlm, LocalLlmOptions, ModelId, ModelStore, SidecarState,
    SpawnHook, Verdict,
};
use ic_widget::ambient::reflection::RunWatch;
use ic_widget::ambient::{AmbientConfig, AmbientService, Suggestion, SuggestionKind};
use ic_widget::canvas::{CallbackSink, CanvasServer};
use ic_widget::character::{self, CharacterInputs, CharacterState};
use ic_widget::error::Error;
use ic_widget::gateway_client::{
    ClientActionId, GateRef, GateResolution, GatewayClient, GatewayEvent, ProjectionItem, RunId,
    ThreadId,
};
use ic_widget::hit_test::HitMask;
use ic_widget::settings::{
    AmbientSettings, CharacterId, ProviderSelection, QuietHours, ReplyMode, SettingsStore,
};
use ic_widget::supervisor::{GatewayConfig, GatewayState, GatewaySupervisor};
use ic_widget::window_state::{LayoutHash, MonitorInfo, WindowPosition};
use ic_widget::{BrowserSidecar, ProcessJob, RunPhase, SecretStore, WindowState, WindowStateStore};
use serde::Serialize;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tokio::sync::Mutex;

const WIDGET: &str = "widget";
const DASHBOARD: &str = "dashboard";
const CANVAS: &str = "canvas";

/// Shared state, owned by Tauri.
struct AppState {
    /// `None` until the gateway becomes ready. Commands answer with a friendly
    /// message until then rather than blocking the UI.
    gateway: Mutex<Option<GatewaySupervisor>>,
    /// One job for every child process the app ever spawns.
    ///
    /// Never read. Held so the job object outlives every supervisor: closing its
    /// last handle is what kills the children, so the app state must keep one
    /// for as long as the app exists.
    #[allow(dead_code, reason = "held for its Drop; see the doc comment")]
    job: Arc<ProcessJob>,
    /// The SSE pump for the active thread, replaced when the thread changes.
    ///
    /// A `tokio` handle rather than a Tauri one: `tauri::async_runtime::spawn`
    /// requires the future to be `Sync`, and the reconnecting `EventStream`
    /// holds a boxed response body that is `Send` but not `Sync`. This is only
    /// ever spawned from inside an async command, so the runtime is present.
    pump: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Wake-phrase recordings banked by the setup wizard, until it trains them.
    wake_takes: Mutex<Vec<Vec<f32>>>,
    /// The conversation both webviews are looking at.
    ///
    /// Owned here, not in a webview: the widget and the dashboard are two views of
    /// **one** conversation. If each created its own thread the user would type
    /// into one and watch the other. See [`current_thread`].
    thread: Mutex<Option<ic_widget::gateway_client::ThreadId>>,
    /// The local model, once one has been brought up. `None` when no model is
    /// installed or the launch failed — the gateway then runs without local
    /// inference. Held for its `Drop`, which stops the sidecar and its proxy;
    /// the sidecar also rides in `job`, so a hard kill takes it down too.
    local_llm: Mutex<Option<LocalLlm>>,
    /// The browser MCP sidecar. `None` when no Chrome/Edge was found or the
    /// sidecar failed to start — the agent then simply has no browser tools.
    /// Held for its `Drop`; it also rides in `job`, so a hard kill of the widget
    /// takes the sidecar *and* its automation browser down with it.
    browser: Mutex<Option<BrowserSidecar>>,
    /// The in-process canvas MCP server. `None` when its port could not be bound.
    /// Held for its `Drop`, which aborts the serve task. It runs in-process (not a
    /// child), so it needs no job enlistment.
    canvas: Mutex<Option<CanvasServer>>,
    /// The most recent canvas render, so the window can fetch it on mount. A
    /// `canvas://render` event emitted while the shell is still loading would be
    /// lost; the shell reads this once via `canvas_content`, then listens live. A
    /// `std` mutex: written from a render, read from a command, never across an
    /// await.
    last_canvas: std::sync::Mutex<Option<ic_widget::canvas::RenderRequest>>,
    /// The in-flight model download, if any: its model id and task handle. One
    /// at a time. Aborting the handle stops the transfer; the partial `.part`
    /// file survives and resumes on the next attempt, so an abort is a safe
    /// pause, not corruption. The id lets a cancel report which download ended.
    download: Mutex<Option<(String, tokio::task::JoinHandle<()>)>>,
    window_state: std::sync::Mutex<WindowState>,
    window_store: WindowStateStore,
    /// Persisted user settings — the active provider today.
    settings_store: SettingsStore,
    /// The character's animation inputs and last emitted state, so a `character://state`
    /// event fires only when the derived state actually changes.
    character: Mutex<CharacterTracker>,
    /// The UI's latest click-through mask. `None` until the first push — the
    /// window is then fully interactive, never stranded click-through.
    ///
    /// A `std` mutex: written from an IPC command, read by the cursor poller,
    /// held only for a lookup — never across an await.
    hit_mask: std::sync::Mutex<Option<HitMask>>,
    /// The voice pipeline, when enabled and provisioned. `None` when voice is off,
    /// still provisioning, or unavailable (no mic / failed model load). Held for
    /// its `Drop`, which stops the loop and releases the mic; Piper also rides in
    /// `job`, so a hard kill takes it down too.
    voice: Mutex<Option<ic_widget::voice::VoiceService>>,
    /// Serialises every load-modify-save of `settings_store`: Tauri commands run
    /// concurrently, and two unsynchronised saves interleave as lost updates (a
    /// tray mute racing a dashboard toggle silently reverted one of them). A `std`
    /// mutex, held only across synchronous file IO — never an await.
    settings_write: std::sync::Mutex<()>,
    /// The proactive side of the app (Phase 7a). `None` while ambient mode is off —
    /// which is the default, and which also means the gateway is running without its
    /// trigger poller, so nothing can run a turn nobody asked for.
    ambient: Mutex<Option<Arc<AmbientService>>>,
    /// The automation watch loop. Aborted when ambient mode is switched off.
    ambient_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The ambient thread — the conversation the *app* starts, kept apart from the
    /// user's so a turn they never asked for cannot land in their transcript.
    ambient_thread: Mutex<Option<ThreadId>>,
    /// Which chat runs have already earned a reflection turn (Phase 7b). One watch
    /// for the whole app: run ids are unique, so it survives thread switches.
    reflection_runs: Mutex<RunWatch>,
    /// The skill import awaiting its bubble answer (Phase 7c), if any. One at a
    /// time — a new request replaces an unanswered one. Lives outside the
    /// ambient service on purpose: an import is *solicited*, so it must work
    /// with ambient off and must never spend a guardrail slot.
    pending_import: Mutex<Option<PendingImport>>,
    /// The most recently cloned skills repo (Phase 8e), kept alive so an approved
    /// install can copy its bundle. Replaced on each new clone.
    repo_clone: Mutex<Option<RepoClone>>,
    /// The ambient watcher loop (Phase 7d). Spawned with ambient mode, aborted
    /// with it — with the master switch off, no signal is even sampled.
    watcher_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// A reviewed skill import waiting for the user's yes or no.
struct PendingImport {
    /// The suggestion id the bubble answers with.
    id: String,
    /// The folder being imported, whose bundle files ride along. `None` for a
    /// draft with no folder behind it — a "study this repo" result (Phase 8e) is
    /// text the model wrote, not a directory someone shipped, so it installs as
    /// text alone.
    folder: Option<PathBuf>,
    /// The skill's validated name (the install directory).
    name: String,
    /// The exact reviewed SKILL.md text — what an approval installs, verbatim.
    skill_md: String,
    /// Whether this replaces a same-named installed skill (a git re-sync). The
    /// existing skill is removed first, so the update is not refused as a
    /// duplicate. `false` for a first install (7c folder imports are always this).
    overwrite: bool,
}

/// A cloned skills repo, held in app state so its temp directory outlives the
/// review: the bundle files are copied from it only when the user approves an
/// install. Replacing it drops the old `TempDir`, which deletes the old clone.
struct RepoClone {
    /// Keeps the clone on disk until this is dropped.
    _dir: tempfile::TempDir,
    /// The skills found, with their folders inside `_dir`.
    import: ic_widget::git_import::RepoImport,
}

/// The inputs the character state derives from, plus the last state emitted.
#[derive(Default)]
struct CharacterTracker {
    inputs: CharacterInputs,
    last: Option<CharacterState>,
}

impl AppState {
    /// A client for the running gateway, or a message explaining why not.
    async fn client(&self) -> Result<GatewayClient, String> {
        match &*self.gateway.lock().await {
            Some(gateway) => Ok(gateway.client().clone()),
            None => Err("The agent is still starting. Give it a moment.".into()),
        }
    }

    /// Atomically load-modify-save the settings, returning the saved copy. Every
    /// mutation goes through here so concurrent commands can't clobber each
    /// other's fields.
    fn update_settings(
        &self,
        mutate: impl FnOnce(&mut ic_widget::settings::Settings),
    ) -> Result<ic_widget::settings::Settings, String> {
        let _guard = self
            .settings_write
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut settings = self.settings_store.load().map_err(|e| e.to_string())?;
        mutate(&mut settings);
        self.settings_store
            .save(&settings)
            .map_err(|e| e.to_string())?;
        Ok(settings)
    }
}

/// What the UI sees. Never a raw HTTP status: the UI has no business rendering
/// one, and the gateway's own taxonomy is the thing worth translating.
fn user_facing(error: Error) -> String {
    if error.is_stream_cap() {
        return "The agent is busy. Try again in a moment.".into();
    }
    if error.is_duplicate_action() {
        // The message was accepted exactly once. Not a failure.
        return "That message was already sent.".into();
    }
    match error {
        Error::Http { .. } => "Lost contact with the agent.".into(),
        Error::Gateway { .. } => "The agent refused that request.".into(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------- commands

#[tauri::command]
async fn gateway_state(state: tauri::State<'_, AppState>) -> Result<GatewayState, String> {
    Ok(match &*state.gateway.lock().await {
        Some(gateway) => gateway.state(),
        None => GatewayState::Starting,
    })
}

#[tauri::command]
async fn gateway_log(state: tauri::State<'_, AppState>) -> Result<String, String> {
    Ok(match &*state.gateway.lock().await {
        Some(gateway) => gateway.output_tail(),
        None => String::new(),
    })
}

/// The conversation both windows are looking at, creating it on first ask.
///
/// **The thread is owned by Rust, not by a webview.** The widget and the dashboard
/// are two independent webviews showing *one* conversation; if each created its
/// own thread on mount, the user would type into one and watch the other, and the
/// second pump would abort the first (the gateway caps concurrent streams anyway).
/// So whoever asks first creates it, and everyone else joins.
#[tauri::command]
async fn current_thread(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let mut current = state.thread.lock().await;
    if let Some(thread_id) = current.as_ref() {
        return Ok(thread_id.to_string());
    }
    let thread_id = open_thread(&app, &state).await?;
    *current = Some(thread_id.clone());
    Ok(thread_id.to_string())
}

/// Start a fresh conversation, replacing the current one in both windows.
#[tauri::command]
async fn new_thread(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<String, String> {
    let thread_id = open_thread(&app, &state).await?;
    *state.thread.lock().await = Some(thread_id.clone());
    // Both webviews follow the app's thread, so tell them it moved rather than
    // leaving the widget bubbling replies from a conversation the user has left.
    let _ = app.emit("thread://changed", thread_id.to_string());
    Ok(thread_id.to_string())
}

/// Create a thread on the gateway and point the event pump at it.
async fn open_thread(
    app: &AppHandle,
    state: &AppState,
) -> Result<ic_widget::gateway_client::ThreadId, String> {
    let client = state.client().await?;
    let thread_id = client.create_thread().await.map_err(user_facing)?;

    // One pump at a time. A stale pump would emit events for a thread the UI no
    // longer shows, and the gateway caps us at three concurrent streams.
    let mut pump = state.pump.lock().await;
    if let Some(previous) = pump.take() {
        previous.abort();
    }
    *pump = Some(tokio::spawn(pump_events(
        app.clone(),
        client,
        thread_id.clone(),
    )));

    // A fresh thread has no active run; clear any phase left from the last one.
    update_character(app, |inputs| inputs.run = None).await;

    tracing::info!(%thread_id, "the widget opened a thread and started its event pump");
    Ok(thread_id)
}

#[derive(Serialize)]
struct SendResult {
    run_id: String,
}

#[tauri::command]
async fn send_message(
    state: tauri::State<'_, AppState>,
    thread_id: String,
    content: String,
) -> Result<SendResult, String> {
    let client = state.client().await?;
    let thread_id = ThreadId::new(thread_id).map_err(user_facing)?;
    let outcome = client
        .send_message(&thread_id, &content, &ClientActionId::new())
        .await
        .map_err(user_facing)?;
    Ok(SendResult {
        run_id: outcome.run_id().to_string(),
    })
}

/// What the UI hears back from Stop.
///
/// Both fields mean "there is nothing left to stop" — the difference is only
/// *why*, and neither is an error the user should see.
#[derive(Serialize)]
struct CancelResult {
    /// The run had already finished. The common case: the reply lands while the
    /// click is in the air.
    already_terminal: bool,
    /// The gateway has never heard of this run — a stale id the UI was holding.
    /// Refresh, do not complain.
    unknown: bool,
}

/// Stop the run.
///
/// **Verified semantics (against the running gateway):** this asks the gateway to
/// cancel — the response says `CancelRequested`, and the run goes terminal on the
/// projection stream shortly after. It does **not** abort the in-flight request to
/// the model: a local `llama-server` keeps generating to completion, so Stop means
/// "stop showing me this", not "stop computing this". See the Phase 8a notes.
#[tauri::command]
async fn cancel_run(
    state: tauri::State<'_, AppState>,
    thread_id: String,
    run_id: String,
) -> Result<CancelResult, String> {
    let client = state.client().await?;
    let thread_id = ThreadId::new(thread_id).map_err(user_facing)?;
    let run_id = RunId::new(run_id).map_err(user_facing)?;
    match client.cancel_run(&thread_id, &run_id).await {
        Ok(outcome) => Ok(CancelResult {
            already_terminal: outcome.already_terminal,
            unknown: false,
        }),
        // A run that no longer exists cannot be stopped, and saying so in red
        // would be theatre: the user asked for it to not be running, and it is
        // not running.
        Err(error) if error.is_not_found() => {
            tracing::debug!(%run_id, "stop: the gateway does not know this run; refreshing");
            Ok(CancelResult {
                already_terminal: true,
                unknown: true,
            })
        }
        Err(error) => Err(user_facing(error)),
    }
}

#[derive(Serialize)]
struct UiMessage {
    sequence: u64,
    kind: String,
    content: Option<String>,
}

/// The assistant's reply lives here and nowhere else — it is never on the event
/// stream. See `docs/desktop/chat-rendering.md`.
#[tauri::command]
async fn fetch_timeline(
    state: tauri::State<'_, AppState>,
    thread_id: String,
) -> Result<Vec<UiMessage>, String> {
    let client = state.client().await?;
    let thread_id = ThreadId::new(thread_id).map_err(user_facing)?;
    let timeline = client
        .timeline(&thread_id, None)
        .await
        .map_err(user_facing)?;
    Ok(timeline
        .messages
        .into_iter()
        .map(|message| UiMessage {
            sequence: message.sequence,
            kind: format!("{:?}", message.kind).to_lowercase(),
            content: message.content,
        })
        .collect())
}

#[tauri::command]
async fn resolve_gate(
    state: tauri::State<'_, AppState>,
    thread_id: String,
    run_id: String,
    gate_ref: String,
    approved: bool,
) -> Result<(), String> {
    let client = state.client().await?;
    let thread_id = ThreadId::new(thread_id).map_err(user_facing)?;
    let run_id = RunId::new(run_id).map_err(user_facing)?;
    let gate_ref = GateRef::new(gate_ref).map_err(user_facing)?;
    // "Always allow" is not offered: the facade refuses persistent approvals.
    let resolution = if approved {
        GateResolution::Approved
    } else {
        GateResolution::Denied
    };
    client
        .resolve_gate(&thread_id, &run_id, &gate_ref, resolution)
        .await
        .map_err(user_facing)
}

/// Answer a browser sensitive-fill approval request.
///
/// The `id` comes from the `browser://approval` event. Clearing the character's
/// concerned state here is optimistic — if more approvals are queued the next
/// event re-sets it — but a single pending fill is by far the common case.
#[tauri::command]
async fn answer_browser_fill(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    id: u64,
    approved: bool,
) -> Result<(), String> {
    let answered = match &*state.browser.lock().await {
        Some(sidecar) => sidecar.answer_fill(id, approved).await,
        // No sidecar means nothing is waiting; the fill already denied on timeout.
        None => Ok(()),
    };
    update_character(&app, |inputs| inputs.browser_approval_pending = false).await;
    answered
}

// ----------------------------------------------------------- dashboard panels

/// A row in the chats panel.
#[derive(Serialize)]
struct UiThread {
    thread_id: String,
    /// `None` until the agent has titled the thread; the UI shows a placeholder.
    title: Option<String>,
    /// Whether the user has hidden this conversation. See
    /// [`ic_widget::settings::Settings::hidden_threads`] — it is a local archive,
    /// not a delete, because no route deletes a thread.
    hidden: bool,
    /// Whether this is the conversation both windows are currently showing.
    current: bool,
}

/// One page of the chats list.
#[derive(Serialize)]
struct UiThreadPage {
    threads: Vec<UiThread>,
    /// Pass back to page. `None` is the end of the list — the gateway omits the
    /// field entirely rather than sending null, which the client normalizes.
    next_cursor: Option<String>,
}

/// The caller's threads, newest first. Threads survive gateway restarts (they
/// are persisted through the libSQL-backed root filesystem), so this is a stable
/// list, not a per-session one.
#[tauri::command]
async fn list_threads(
    state: tauri::State<'_, AppState>,
    limit: Option<u32>,
    cursor: Option<String>,
) -> Result<UiThreadPage, String> {
    let client = state.client().await?;
    let page = client
        .list_threads_page(limit, cursor.as_deref())
        .await
        .map_err(user_facing)?;

    // silent-ok: an unreadable settings file means nothing is hidden, which
    // shows *more* than it should rather than losing a conversation.
    let hidden = state
        .settings_store
        .load()
        .map(|settings| settings.hidden_threads)
        .unwrap_or_default();
    let current = state.thread.lock().await.as_ref().map(ThreadId::to_string);

    Ok(UiThreadPage {
        threads: page
            .threads
            .into_iter()
            .map(|thread| {
                let id = thread.thread_id.to_string();
                UiThread {
                    hidden: hidden.contains(&id),
                    current: current.as_deref() == Some(id.as_str()),
                    thread_id: id,
                    title: thread.title,
                }
            })
            .collect(),
        next_cursor: page.next_cursor,
    })
}

/// Hide a conversation from the list, or bring it back.
///
/// **This is not a delete, and the button does not say it is.** No serve route
/// removes a thread, and we do not touch IronClaw's database — so the
/// conversation is still there, in the gateway, exactly as the agent left it.
/// The widget simply stops listing it.
#[tauri::command]
async fn set_thread_hidden(
    state: tauri::State<'_, AppState>,
    thread_id: String,
    hidden: bool,
) -> Result<(), String> {
    // Validate before persisting: an id that is not a thread id would sit in the
    // settings file forever, hiding nothing.
    let thread_id = ThreadId::new(thread_id).map_err(user_facing)?.to_string();
    state.update_settings(|settings| {
        settings.hidden_threads.retain(|id| id != &thread_id);
        if hidden {
            settings.hidden_threads.push(thread_id.clone());
        }
    })?;
    Ok(())
}

/// Point both windows at an existing conversation, and follow it.
///
/// The thread is owned by Rust (see [`current_thread`]), so switching means
/// replacing the app's thread and its event pump — the same move
/// [`respond_suggestion`] makes when the user accepts "show me".
#[tauri::command]
async fn use_thread(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    thread_id: String,
) -> Result<(), String> {
    let thread_id = ThreadId::new(thread_id).map_err(user_facing)?;
    // Prove the thread exists before repointing the app at it: a stale id from a
    // list the user has been staring at for ten minutes must not strand both
    // windows on a conversation the gateway has never heard of.
    let client = state.client().await?;
    client
        .timeline(&thread_id, Some(1))
        .await
        .map_err(user_facing)?;
    follow_thread(&app, &state, thread_id).await?;
    Ok(())
}

/// Repoint the app's thread and event pump at `thread_id`, and tell both windows.
async fn follow_thread(
    app: &AppHandle,
    state: &AppState,
    thread_id: ThreadId,
) -> Result<(), String> {
    let client = state.client().await?;
    let mut pump = state.pump.lock().await;
    if let Some(previous) = pump.take() {
        previous.abort();
    }
    *pump = Some(tokio::spawn(pump_events(
        app.clone(),
        client,
        thread_id.clone(),
    )));
    drop(pump);

    *state.thread.lock().await = Some(thread_id.clone());
    update_character(app, |inputs| inputs.run = None).await;
    let _ = app.emit("thread://changed", thread_id.to_string());
    Ok(())
}

/// A row in the automations panel.
#[derive(Serialize)]
struct UiAutomation {
    automation_id: String,
    name: String,
    /// Snake_case state (`scheduled`, `paused`, …), rendered as a badge.
    state: String,
    next_run_at: Option<String>,
    last_run_at: Option<String>,
    /// `ok` / `error` / a raw status, or `None` before the first run.
    last_status: Option<String>,
    is_active: bool,
}

/// The caller's scheduled automations. These are schedule entries, **not** run
/// history — no run-history route exists (see `docs/desktop/dashboard-gaps.md`).
#[tauri::command]
async fn list_automations(state: tauri::State<'_, AppState>) -> Result<Vec<UiAutomation>, String> {
    let client = state.client().await?;
    let automations = client.list_automations(None).await.map_err(user_facing)?;
    Ok(automations
        .into_iter()
        .map(|automation| UiAutomation {
            automation_id: automation.automation_id,
            name: automation.name,
            state: automation.state.as_str().to_string(),
            next_run_at: automation.next_run_at,
            last_run_at: automation.last_run_at,
            last_status: automation
                .last_status
                .as_ref()
                .map(|status| status.as_str().to_string()),
            is_active: automation.is_active,
        })
        .collect())
}

/// The local model panel's data. `None` when no local model is running.
#[derive(Serialize)]
struct UiModel {
    /// The name the model answers to.
    model_id: String,
    /// Which llama.cpp build is serving it (`vulkan`, `cuda12`, `cpu`).
    backend: String,
    /// The sidecar's live state — `{ state: "ready" }`, `{ state: "suspect",
    /// reason }`, etc. The panel renders it as a badge.
    sidecar: SidecarState,
    /// Layers offloaded to the GPU, out of `block_count`. Equal means full
    /// offload (plus the output tensors when it exceeds `block_count`).
    n_gpu_layers: u32,
    /// The model's transformer layer count.
    block_count: u32,
    /// `full_offload` / `partial_offload` / `cpu_only` / `refused`.
    verdict: String,
    /// Estimated VRAM the server consumes, in MiB.
    estimated_vram_mb: u64,
    /// Estimated host RAM the server consumes, in MiB.
    estimated_host_mb: u64,
    /// Placement advisories, already human-readable.
    warnings: Vec<String>,
    /// What the proxy has actually seen: tokens/sec, token counts, failovers.
    /// Everything above is a decision made at launch; this is the only part that
    /// moves while the model runs.
    metrics: ic_llama::Metrics,
    /// The cloud provider this model falls back to, if one is configured.
    fallback: Option<String>,
}

/// Read-only status of the running local model. Reflects placement decided at
/// launch plus the sidecar's live health; `None` when the app is running
/// without local inference.
#[tauri::command]
async fn local_model_status(state: tauri::State<'_, AppState>) -> Result<Option<UiModel>, String> {
    let guard = state.local_llm.lock().await;
    let Some(llm) = guard.as_ref() else {
        return Ok(None);
    };
    let placement = llm.placement();
    Ok(Some(UiModel {
        model_id: llm.sidecar().model_id().to_string(),
        backend: llm.backend().as_str().to_string(),
        sidecar: llm.sidecar().state(),
        n_gpu_layers: placement.n_gpu_layers,
        block_count: llm.model().gguf.block_count,
        verdict: verdict_label(&placement.verdict).to_string(),
        estimated_vram_mb: placement.estimated_vram_bytes / (1024 * 1024),
        estimated_host_mb: placement.estimated_host_bytes / (1024 * 1024),
        warnings: placement
            .warnings
            .iter()
            .map(|warning| warning.to_string())
            .collect(),
        metrics: llm.metrics(),
        fallback: state
            .settings_store
            .load()
            .ok()
            .and_then(|settings| settings.cloud_fallback)
            .map(|fallback| fallback.id),
    }))
}

/// The stable wire label for a placement verdict.
fn verdict_label(verdict: &Verdict) -> &'static str {
    match verdict {
        Verdict::FullOffload => "full_offload",
        Verdict::PartialOffload => "partial_offload",
        Verdict::CpuOnly => "cpu_only",
        Verdict::Refuse { .. } => "refused",
    }
}

// -------------------------------------------------------------- provider keys

/// A configurable cloud provider, as the settings panel sees it.
#[derive(Serialize)]
struct UiProvider {
    /// The `LLM_BACKEND` id.
    id: String,
    /// One-line description from the catalog.
    description: String,
    /// The model used unless the user overrides it.
    default_model: String,
    /// Whether an API key is stored for it. **Never the key itself.**
    has_key: bool,
    /// Whether it can serve as the local model's cloud fallback. False for
    /// providers whose wire dialect is not OpenAI-shaped: the proxy forwards
    /// the body the gateway already built, so they could never answer it.
    can_fail_over: bool,
    /// The vendor's own name for itself.
    name: String,
    /// Where the user goes to mint a key.
    key_url: Option<String>,
    /// The endpoint we will probe and the gateway will use. `None` means the
    /// user must supply one (`openai_compatible`, self-hosted).
    base_url: Option<String>,
    /// Whether "Test" can actually answer for this provider. False for the
    /// out-of-band authenticators — better to say so than to show a green tick
    /// that means nothing (which is what the gateway's own probe does).
    probeable: bool,
}

/// The provider panel's data: what is active, and the configurable providers.
#[derive(Serialize)]
struct UiProviderSettings {
    /// The selection the gateway is running on.
    active: ProviderSelection,
    /// The cloud providers that take an API key. The local model is a separate,
    /// always-available choice the UI offers alongside these.
    providers: Vec<UiProvider>,
    /// The cloud provider the local model falls back to, if any.
    fallback: Option<ic_widget::settings::FallbackProvider>,
}

/// The active selection and the configurable cloud providers, each flagged with
/// whether a key is stored. The key values never leave the credential store.
#[tauri::command]
async fn provider_settings(
    state: tauri::State<'_, AppState>,
) -> Result<UiProviderSettings, String> {
    let settings = state.settings_store.load().map_err(user_facing)?;

    let secrets = SecretStore::new();
    let catalog = ic_widget::providers::api_key_providers().map_err(user_facing)?;
    let mut providers = Vec::with_capacity(catalog.len());
    for provider in catalog {
        // A keyring read failure is surfaced, not folded into "no key" — that
        // would let a transient store error read as an unconfigured provider.
        let has_key = secrets.has_provider_key(&provider).map_err(user_facing)?;
        providers.push(UiProvider {
            name: provider.display_name().to_string(),
            key_url: provider.key_url().map(str::to_string),
            base_url: provider.probe_base_url(),
            probeable: provider.is_probeable(),
            id: provider.id.clone(),
            description: provider.description.clone(),
            default_model: provider.default_model.clone(),
            has_key,
            can_fail_over: provider.can_fail_over(),
        });
    }
    Ok(UiProviderSettings {
        active: settings.active_provider,
        providers,
        fallback: settings.cloud_fallback,
    })
}

// ------------------------------------------------------------- connectors

/// One row in the Connectors panel (Phase 8b).
///
/// Merged from two routes, because neither alone is enough: the registry says
/// what *can* be installed, and `/extensions` says what *is* — with its phase,
/// its published capabilities, and what it still needs.
#[derive(Serialize)]
struct UiConnector {
    id: String,
    name: String,
    description: String,
    /// `wasm_tool`, `mcp_server`, or `first_party`.
    kind: Option<String>,
    installed: bool,
    /// `installed` (present) vs `active` (its tools actually reach the model).
    active: bool,
    /// How many tools it gives the agent. Zero on an active connector means
    /// something is wrong, whatever the activate call claimed (the Phase 4 trap).
    tool_count: usize,
    /// The auth provider its credential belongs to, e.g. `github`.
    provider: Option<String>,
    /// `manual_token` — the user pastes a string, which the panel can do. `oauth` —
    /// it cannot: the gateway will not even start that flow without a Google/Notion
    /// OAuth client, which only a human can register with the vendor. The panel
    /// branches on this rather than offering a token box that could never work.
    auth_kind: Option<String>,
    /// Whether every required secret has been provided.
    ready: bool,
    /// The vendor's own words: what the credential is, where to get it.
    instructions: Option<String>,
    setup_url: Option<String>,
}

/// The connectors: what the gateway offers, and what is installed.
#[tauri::command]
async fn list_connectors(state: tauri::State<'_, AppState>) -> Result<Vec<UiConnector>, String> {
    let client = state.client().await?;
    let registry = client.connector_registry().await.map_err(user_facing)?;
    let installed = client.installed_extensions().await.map_err(user_facing)?;

    let mut connectors = Vec::with_capacity(registry.len());
    for entry in registry {
        let id = entry.package.id.clone();
        let live = installed
            .iter()
            .find(|extension| extension.package.id == id);

        // A connector is only *ready* when every required secret is provided —
        // and the setup projection is the only place that says so honestly. It is
        // also the only place that says *how* the credential is obtained.
        let (provider, auth_kind, ready) = match live {
            Some(_) => {
                let setup = client.extension_setup(&id).await.ok();
                let secrets = setup.map(|setup| setup.secrets).unwrap_or_default();
                let provider = secrets.iter().find_map(|secret| secret.provider.clone());
                let auth_kind = secrets
                    .iter()
                    .find_map(|secret| secret.setup.as_ref()?.kind.clone());
                let ready = secrets
                    .iter()
                    .all(|secret| secret.provided || secret.optional);
                (provider, auth_kind, ready)
            }
            None => (None, None, false),
        };

        let onboarding = live.and_then(|extension| extension.onboarding.clone());

        connectors.push(UiConnector {
            name: entry.display_name.clone().unwrap_or_else(|| id.clone()),
            description: entry.description.clone().unwrap_or_default(),
            kind: entry.kind.clone(),
            installed: live.is_some(),
            active: live.is_some_and(|extension| extension.active),
            tool_count: live.map(|extension| extension.tools.len()).unwrap_or(0),
            provider,
            auth_kind,
            ready,
            instructions: onboarding
                .as_ref()
                .and_then(|copy| copy.credential_instructions.clone()),
            setup_url: onboarding.as_ref().and_then(|copy| copy.setup_url.clone()),
            id,
        });
    }
    Ok(connectors)
}

/// Install a connector. Returns the vendor's onboarding copy, which the panel
/// renders rather than inventing its own.
#[derive(Serialize)]
struct UiInstallOutcome {
    awaiting_token: bool,
    message: Option<String>,
    instructions: Option<String>,
    setup_url: Option<String>,
    next_step: Option<String>,
}

#[tauri::command]
async fn install_connector(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<UiInstallOutcome, String> {
    let client = state.client().await?;
    let outcome = client.install_extension(&id).await.map_err(user_facing)?;
    let onboarding = outcome
        .onboarding
        .unwrap_or(ic_widget::gateway_client::Onboarding {
            credential_instructions: None,
            credential_next_step: None,
            setup_url: None,
            instructions: None,
        });
    Ok(UiInstallOutcome {
        awaiting_token: outcome.awaiting_token,
        message: outcome.message,
        instructions: onboarding
            .credential_instructions
            .or(onboarding.instructions),
        setup_url: onboarding.setup_url,
        next_step: onboarding.credential_next_step,
    })
}

/// Save a connector's credential.
///
/// **Not `settings.json`.** The token goes straight to the gateway's own secrets
/// vault through the product-auth lane and is never written by us; the widget
/// keeps no copy. Activation follows, because a credential the agent cannot use
/// yet is not a finished job.
#[tauri::command]
async fn set_connector_token(
    state: tauri::State<'_, AppState>,
    provider: String,
    id: String,
    token: String,
) -> Result<bool, String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("Paste the token first.".to_string());
    }
    let client = state.client().await?;
    client
        .store_connector_token(&provider, token)
        .await
        .map_err(user_facing)?;
    // "After saving the token, activate to publish its tools" — the gateway's own
    // instruction, so do it rather than leaving the user a second button.
    let activated = client.activate_extension(&id).await.map_err(user_facing)?;
    Ok(activated)
}

/// Clear the auth gate a run is parked on, so the question can be asked again.
///
/// The fix-it path for the state the user actually hits: they connected GitHub
/// with a token that has since expired, asked a question, and the agent stopped.
/// One button: store the new credential, then drop the run that is waiting on the
/// old one. The frontend still holds the question and re-sends it, and this time
/// the tool call succeeds.
///
/// This is deliberately not the documented gate-resume route. See
/// `GatewayClient::recover_auth_gate` for why that one could not be made to
/// answer, and what is proven in its place.
#[tauri::command]
async fn recover_auth_gate(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    provider: String,
    token: String,
    thread_id: String,
    run_id: String,
) -> Result<(), String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("Paste the token first.".to_string());
    }
    let client = state.client().await?;
    let thread_id = ThreadId::new(thread_id).map_err(user_facing)?;
    let run_id = RunId::new(run_id).map_err(user_facing)?;

    client
        .recover_auth_gate(&provider, token, &thread_id, &run_id)
        .await
        .map_err(user_facing)?;
    update_character(&app, |inputs| inputs.auth_gate_pending = false).await;
    Ok(())
}

/// Publish a connector's tools to the agent, or take them away.
///
/// Disable is `remove`, which is the only lever the API offers — but a removed
/// connector keeps its stored credential, so re-enabling is one click and not a
/// re-setup.
#[tauri::command]
async fn set_connector_enabled(
    state: tauri::State<'_, AppState>,
    id: String,
    enabled: bool,
) -> Result<(), String> {
    let client = state.client().await?;
    match enabled {
        true => {
            client.install_extension(&id).await.map_err(user_facing)?;
            client.activate_extension(&id).await.map_err(user_facing)?;
        }
        false => client.remove_extension(&id).await.map_err(user_facing)?,
    }
    Ok(())
}

// ------------------------------------------------- connector OAuth (Phase 8b.1)

/// The Google OAuth client's state, and the redirect URI the user must register.
#[derive(Serialize)]
struct GoogleOAuthStatus {
    /// Whether a client id + secret are stored — i.e. OAuth connectors can start.
    configured: bool,
    /// The redirect URI to register with Google, byte-for-byte. Shown with a copy
    /// button so the user pastes exactly this into their OAuth client.
    redirect_uri: String,
    /// The fixed loopback port it lands on.
    port: u16,
}

/// The Google OAuth client's state and the exact redirect URI to register.
///
/// The redirect URI is derived from the fixed callback port; the user registers
/// it with Google once and it survives relaunches (the gateway's own port does
/// not). Never returns the stored client id/secret to the webview.
#[tauri::command]
async fn google_oauth_status(
    state: tauri::State<'_, AppState>,
) -> Result<GoogleOAuthStatus, String> {
    let port = state
        .settings_store
        .load()
        .map(|settings| settings.google_oauth.callback_port)
        // silent-ok: unreadable settings mean the default port, same as fresh.
        .unwrap_or(51789);
    let configured = SecretStore::new().has_google_oauth().map_err(user_facing)?;
    Ok(GoogleOAuthStatus {
        configured,
        redirect_uri: ic_widget::oauth_callback::redirect_uri(port),
        port,
    })
}

/// Store the Google OAuth client the user created, then restart the gateway so
/// `serve` boots with it (it reads the OAuth environment once, at startup).
#[tauri::command]
async fn set_google_oauth(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    client_id: String,
    client_secret: String,
) -> Result<(), String> {
    SecretStore::new()
        .set_google_oauth(client_id.trim(), client_secret.trim())
        .map_err(user_facing)?;
    // silent-ok: unreadable settings mean the default (local) provider, which is
    // the right thing to restart onto for a fresh install.
    let selection = state
        .settings_store
        .load()
        .map(|settings| settings.active_provider)
        .unwrap_or_default();
    restart_gateway(app, selection).await;
    Ok(())
}

/// Forget the Google OAuth client and restart the gateway without it.
#[tauri::command]
async fn clear_google_oauth(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    SecretStore::new()
        .clear_google_oauth()
        .map_err(user_facing)?;
    // silent-ok: unreadable settings mean the default (local) provider, which is
    // the right thing to restart onto.
    let selection = state
        .settings_store
        .load()
        .map(|settings| settings.active_provider)
        .unwrap_or_default();
    restart_gateway(app, selection).await;
    Ok(())
}

/// Change the fixed loopback port the OAuth redirect lands on, and restart the
/// gateway so `serve`'s registered redirect URI matches.
///
/// The user must re-register the new redirect URI with Google — the panel shows
/// it. Restarting is required because the redirect URI is boot-time env for
/// `serve`, exactly like the client id.
#[tauri::command]
async fn set_oauth_callback_port(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    port: u16,
) -> Result<(), String> {
    if port < 1024 {
        return Err("Pick a port above 1023 — low ports need admin rights.".to_string());
    }
    let settings = state.update_settings(|settings| settings.google_oauth.callback_port = port)?;
    restart_gateway(app, settings.active_provider).await;
    Ok(())
}

/// Authorize an OAuth connector (Gmail, Drive, …) end to end (Phase 8b.1).
///
/// The one flow the panel could not offer before: read the connector's OAuth
/// requirement, ask `serve` to begin the flow, open Google's consent page in the
/// user's real browser (Google refuses embedded webviews), catch the redirect on
/// the fixed-port listener, let `serve` complete the token exchange, then confirm
/// the credential landed and publish the connector's tools.
///
/// The listener is armed **before** the browser opens, so a port clash is an
/// error the user sees rather than a browser onto a dead redirect.
#[tauri::command]
async fn authorize_google_connector(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    if !SecretStore::new().has_google_oauth().map_err(user_facing)? {
        return Err(
            "Set up your Google OAuth client first — paste its client id and secret above."
                .to_string(),
        );
    }
    let client = state.client().await?;
    let port = state
        .settings_store
        .load()
        .map(|settings| settings.google_oauth.callback_port)
        .unwrap_or(51789);

    // 1. What the connector's OAuth secret needs: the invocation to authorize
    //    against, the scopes, the account label, and the provider.
    let setup = client.extension_setup(&id).await.map_err(user_facing)?;
    let oauth = setup
        .secrets
        .iter()
        .find(|secret| {
            secret
                .setup
                .as_ref()
                .and_then(|setup| setup.kind.as_deref())
                == Some("oauth")
        })
        .ok_or("This connector does not use OAuth.")?;
    let details = oauth
        .setup
        .as_ref()
        .ok_or("The connector's OAuth setup is missing.")?;
    let invocation_id = details.invocation_id.clone().ok_or(
        "The gateway did not offer an OAuth invocation. Make sure the connector is installed \
         and the Google client is configured, then try again.",
    )?;
    let provider = oauth
        .provider
        .clone()
        .unwrap_or_else(|| "google".to_string());
    let account_label = details
        .account_label
        .clone()
        .unwrap_or_else(|| format!("{id} {provider}"));
    let scopes = details.scopes.clone();

    // 2. Ask serve to begin the flow. This is what answered 503 before a client
    //    was configured; now it returns the Google consent URL.
    let start = client
        .start_extension_oauth(&id, &provider, &account_label, &scopes, &invocation_id)
        .await
        .map_err(user_facing)?;
    let expected_state =
        ic_widget::oauth_callback::state_from_authorization_url(&start.authorization_url)
            .ok_or("The gateway returned an authorization URL without a state parameter.")?;

    // 3. Arm the listener before opening the browser, so a port clash surfaces now.
    let armed = ic_widget::oauth_callback::arm(port, expected_state, client.base_url().to_string())
        .await
        .map_err(|error| error.to_string())?;

    update_character(&app, |inputs| inputs.auth_gate_pending = true).await;

    // 4. Send the user to Google. Their real browser, not a webview — Google
    //    blocks embedded user agents for OAuth.
    if let Err(error) = open_in_browser(&start.authorization_url) {
        update_character(&app, |inputs| inputs.auth_gate_pending = false).await;
        return Err(format!(
            "Could not open your browser to sign in: {error}. You can paste this URL yourself:\n{}",
            start.authorization_url
        ));
    }

    // 5. Wait for the redirect. serve completes the exchange when the listener
    //    proxies the callback into it.
    let outcome = armed
        .wait(std::time::Duration::from_secs(300))
        .await
        .map_err(|error| error.to_string());
    update_character(&app, |inputs| inputs.auth_gate_pending = false).await;

    match outcome? {
        ic_widget::oauth_callback::FlowOutcome::Completed => {}
        ic_widget::oauth_callback::FlowOutcome::ProviderError { reason } => {
            return Err(format!(
                "Google did not authorize the connection ({reason}). You can try again."
            ));
        }
        ic_widget::oauth_callback::FlowOutcome::ServeRejected { status } => {
            return Err(format!(
                "The sign-in did not complete (the agent returned {status}). \
                 Check that the redirect URI you registered with Google matches exactly, \
                 then try again."
            ));
        }
    }

    // 6. Confirm the credential actually landed, then publish the tools. The
    //    setup projection is the honest check — the callback returning 2xx says
    //    serve accepted it, not that the account was stored.
    let mut provided = false;
    for _ in 0..20 {
        let setup = client.extension_setup(&id).await.map_err(user_facing)?;
        if setup
            .secrets
            .iter()
            .all(|secret| secret.provided || secret.optional)
        {
            provided = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
    if !provided {
        return Err(
            "Signed in, but the credential did not appear. Refresh and try enabling the connector."
                .to_string(),
        );
    }
    client.activate_extension(&id).await.map_err(user_facing)?;
    Ok(())
}

/// Open a URL in the user's default browser (Windows).
///
/// Spawns `rundll32 url.dll,FileProtocolHandler <url>` directly — no shell — so
/// the `&` characters in an OAuth query are never mis-parsed by `cmd`.
fn open_in_browser(url: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        std::process::Command::new("rundll32.exe")
            .arg("url.dll,FileProtocolHandler")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
    #[cfg(not(windows))]
    {
        let _ = url;
        Err("opening a browser is only wired for Windows".to_string())
    }
}

/// Does this key work, and what can it run? (Phase 8a.5)
///
/// Asked of the **provider itself**, from this process. The gateway cannot
/// answer: its `/llm/test-connection` reports `ok` for a dead endpoint with a
/// junk key, and its `/llm/providers` answers `503` under our profile. Both are
/// pinned by integration tests that fail the day upstream fixes them.
///
/// The key is read from the credential store when the user has already saved one
/// (`key: None`), so a "Test" on a configured provider never round-trips the
/// secret through the webview. A `key` is only passed in when the user is typing
/// a new one and wants to check it *before* saving.
#[tauri::command]
async fn test_provider(
    provider_id: String,
    key: Option<String>,
    base_url: Option<String>,
) -> Result<ic_widget::probe::Probe, String> {
    let provider = provider_by_id(&provider_id)?;

    let key = match key
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
    {
        Some(typed) => typed,
        None => SecretStore::new()
            .provider_key(&provider)
            .map_err(user_facing)?
            .ok_or_else(|| "No key saved for this provider yet.".to_string())?,
    };

    Ok(ic_widget::probe::probe(&provider, &key, base_url.as_deref()).await)
}

/// Resolve a provider id from the catalog, or a user-facing error.
fn provider_by_id(id: &str) -> Result<ic_widget::providers::Provider, String> {
    ic_widget::providers::find(id)
        .map_err(user_facing)?
        .ok_or_else(|| "That provider is not in the catalog.".to_string())
}

/// Store an API key for a provider. Does not restart the gateway — the key
/// takes effect on the next [`apply_provider`].
#[tauri::command]
async fn set_provider_key(provider_id: String, key: String) -> Result<(), String> {
    let provider = provider_by_id(&provider_id)?;
    SecretStore::new()
        .set_provider_key(&provider, &key)
        .map_err(user_facing)
}

/// Forget a provider's API key.
#[tauri::command]
async fn clear_provider_key(provider_id: String) -> Result<(), String> {
    let provider = provider_by_id(&provider_id)?;
    SecretStore::new()
        .clear_provider_key(&provider)
        .map_err(user_facing)
}

/// Switch the active provider and restart the gateway onto it.
///
/// Persists the choice first, so a crash mid-restart still comes back on the new
/// provider. Then it tears the current gateway and local model down — freeing
/// the sidecar's VRAM and port — and brings the gateway back up on the new
/// selection. The webviews hold a client bound to the old gateway, so they are
/// reloaded to re-read state and re-establish their thread.
#[tauri::command]
async fn apply_provider(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    selection: ProviderSelection,
) -> Result<(), String> {
    // Load-modify-save (serialised): settings carry more than the provider, and a
    // rebuilt literal here would silently reset the rest.
    state.update_settings(|settings| settings.active_provider = selection.clone())?;
    restart_gateway(app.clone(), selection).await;
    Ok(())
}

/// Pin which installed GGUF the local model runs, and restart onto it.
///
/// `None` unpins — back to "the first usable model". The restart is the same
/// machinery the provider switch uses: the sidecar's model is chosen at launch,
/// so a new choice needs a new sidecar (and the gateway behind it, which holds
/// the proxy URL).
#[tauri::command]
async fn use_model(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    model_id: Option<String>,
) -> Result<(), String> {
    let settings = state.update_settings(|settings| settings.default_model = model_id.clone())?;
    // Only a *local* selection is affected: pinning a GGUF while a cloud provider
    // is active changes what happens when the user switches back, not now.
    if settings.active_provider == ProviderSelection::Local {
        restart_gateway(app, ProviderSelection::Local).await;
    }
    Ok(())
}

/// Choose the cloud provider the local model falls back to, or `None` for none.
///
/// The proxy owns the retry, so this restarts the gateway (the proxy is built
/// with the model). It does **not** change `LLM_BACKEND`: the gateway keeps
/// seeing exactly one provider, and the cloud key never enters its environment.
#[tauri::command]
async fn set_cloud_fallback(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    fallback: Option<ic_widget::settings::FallbackProvider>,
) -> Result<(), String> {
    // Refuse a provider that cannot actually serve a fallback rather than
    // storing a setting that silently never fires.
    if let Some(chosen) = &fallback {
        let provider = ic_widget::providers::find(&chosen.id)
            .map_err(user_facing)?
            .ok_or_else(|| format!("Unknown provider \u{201c}{}\u{201d}.", chosen.id))?;
        if !provider.can_fail_over() {
            return Err(format!(
                "{} cannot be a fallback: it does not speak the OpenAI-compatible API the \
                 local model's proxy forwards.",
                chosen.id
            ));
        }
        if !SecretStore::new()
            .has_provider_key(&provider)
            .map_err(user_facing)?
        {
            return Err(format!(
                "Add an API key for {} first — a fallback with no key would never answer.",
                chosen.id
            ));
        }
    }

    let settings = state.update_settings(|settings| settings.cloud_fallback = fallback.clone())?;
    if settings.active_provider == ProviderSelection::Local {
        restart_gateway(app, ProviderSelection::Local).await;
    }
    Ok(())
}

/// Tear the gateway (and its local model) down and bring it back up on `selection`.
///
/// Shared by the provider switch and the ambient toggle: both change something the
/// runtime reads **once at boot** — `LLM_BASE_URL` for one, the trigger-poller
/// switch for the other — so both need the process replaced, not reconfigured.
async fn restart_gateway(app: AppHandle, selection: ProviderSelection) {
    let state = app.state::<AppState>();

    // Tell the UI we are restarting before the teardown, so the badge is honest
    // during the gap. The old run, if any, is gone with the old gateway.
    let _ = app.emit("gateway://state", GatewayState::Starting);
    update_character(&app, |inputs| {
        inputs.gateway = GatewayState::Starting;
        inputs.run = None;
    })
    .await;

    if let Some(mut gateway) = state.gateway.lock().await.take() {
        gateway.stop().await;
    }
    // Dropping the model stops the sidecar and its proxy, freeing VRAM and the
    // port before the new selection reserves anything.
    drop(state.local_llm.lock().await.take());

    bring_up_gateway(app.clone(), selection).await;

    // The old client the webviews were using is gone. A reload re-runs their
    // mount, which re-reads gateway state and re-creates the thread and its pump
    // against the new gateway.
    reload_webviews(&app);
}

/// Reload every webview, after the gateway behind them has been replaced.
fn reload_webviews(app: &AppHandle) {
    for label in [WIDGET, DASHBOARD] {
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.eval("window.location.reload()");
        }
    }
}

// ------------------------------------------------------------- model download

/// Turn a download failure into something worth showing a user.
///
/// Only the cases with a clear action are translated; the rest fall back to the
/// error's own message, which for this crate is already user-readable.
fn download_error(error: ic_llama::Error) -> String {
    use ic_llama::Error;
    match &error {
        // Windows disk-full os errors: ERROR_DISK_FULL / ERROR_HANDLE_DISK_FULL.
        Error::Io { source, .. } if matches!(source.raw_os_error(), Some(112 | 39)) => {
            "Not enough disk space to finish the download.".to_string()
        }
        // `install` resolves the file on the hub before fetching, so a wrong repo
        // or file name lands here rather than as a raw 404.
        Error::HubResolve { .. } | Error::HttpStatus { status: 404, .. } => {
            "That model was not found on HuggingFace — check the repo and file name.".to_string()
        }
        Error::ChecksumMismatch { .. } => {
            "The download was corrupted. It resumes from where it left off next time.".to_string()
        }
        _ => format!("Download failed: {error}"),
    }
}

/// A model already in the store, for the download panel.
#[derive(Serialize)]
struct UiInstalledModel {
    /// The id, which is the file stem.
    id: String,
    /// Size on disk, in MiB.
    size_mb: u64,
    /// Why it is not auto-loaded, if it is suspect.
    suspect: Option<String>,
}

/// The models the download panel suggests.
#[tauri::command]
fn recommended_models() -> Vec<ic_widget::model_catalog::RecommendedModel> {
    ic_widget::model_catalog::recommended()
}

/// The models already downloaded, newest listing rules aside — sorted by id, as
/// the store returns them.
#[tauri::command]
async fn installed_models() -> Result<Vec<UiInstalledModel>, String> {
    let root = llama_root()?;
    let downloader = Downloader::new().map_err(download_error)?;
    let store = ModelStore::new(&root, downloader);
    let models = store.list().await.map_err(download_error)?;

    let mut out = Vec::with_capacity(models.len());
    for model in models {
        // Size on disk, not the header's weight count: this is what the user is
        // spending. A stat failure drops to 0 rather than failing the listing.
        let size_mb = tokio::fs::metadata(&model.path)
            .await
            .map(|meta| meta.len() / (1024 * 1024))
            .unwrap_or(0); // silent-ok: a missing size still lists the model
        out.push(UiInstalledModel {
            id: model.id.to_string(),
            size_mb,
            suspect: model.suspect,
        });
    }
    Ok(out)
}

/// What the UI receives on `model://event` during a download.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ModelEvent {
    /// Bytes have moved. `fraction` is `None` when the server sent no length.
    Progress {
        id: String,
        downloaded: u64,
        total: Option<u64>,
        fraction: Option<f64>,
    },
    /// The download ended. Exactly one of the flags explains how.
    Finished {
        id: String,
        ok: bool,
        cancelled: bool,
        error: Option<String>,
    },
}

/// Start downloading a model from HuggingFace, streaming progress to the UI.
///
/// Returns as soon as the transfer starts; progress and completion arrive on
/// `model://event`. Only one download runs at a time. The digest is fetched from
/// the hub, so no checksum need be supplied.
#[tauri::command]
async fn download_model(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    repo: String,
    file: String,
) -> Result<(), String> {
    let model = HubModel::new(repo, file);
    let id = model.model_id().map_err(download_error)?.to_string();

    let mut slot = state.download.lock().await;
    if let Some((_, handle)) = slot.as_ref()
        && !handle.is_finished()
    {
        return Err("A download is already in progress.".to_string());
    }

    let root = llama_root()?;
    let handle = tokio::spawn(run_download(app, root, model, id.clone()));
    *slot = Some((id, handle));
    Ok(())
}

/// Cancel the in-flight download. The partial file is kept and resumes on the
/// next attempt.
#[tauri::command]
async fn cancel_download(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    if let Some((id, handle)) = state.download.lock().await.take()
        && !handle.is_finished()
    {
        // Aborting drops the stream and the file handle mid-write. The `.part`
        // survives; the final rename only happens after digest verification, so
        // there is nothing half-installed to clean up.
        handle.abort();
        // The aborted task cannot emit its own terminal event, so report the
        // cancellation here — otherwise the UI waits forever on a download that
        // has stopped.
        let _ = app.emit(
            "model://event",
            ModelEvent::Finished {
                id,
                ok: false,
                cancelled: true,
                error: None,
            },
        );
    }
    Ok(())
}

/// Delete an installed model.
#[tauri::command]
async fn remove_model(id: String) -> Result<(), String> {
    let model_id = ModelId::new(id).map_err(download_error)?;
    let root = llama_root()?;
    let downloader = Downloader::new().map_err(download_error)?;
    ModelStore::new(&root, downloader)
        .remove(&model_id)
        .await
        .map_err(download_error)
}

/// The download task: fetch the model, emitting progress, then report the
/// outcome. Runs until the transfer finishes, fails, or is aborted.
async fn run_download(app: AppHandle, root: PathBuf, model: HubModel, id: String) {
    let downloader = match Downloader::new() {
        Ok(downloader) => downloader,
        Err(error) => {
            let _ = app.emit(
                "model://event",
                ModelEvent::Finished {
                    id,
                    ok: false,
                    cancelled: false,
                    error: Some(download_error(error)),
                },
            );
            return;
        }
    };
    let store = ModelStore::new(&root, downloader);

    let progress: ProgressFn = {
        let app = app.clone();
        let id = id.clone();
        Arc::new(move |snapshot: Progress| {
            let _ = app.emit(
                "model://event",
                ModelEvent::Progress {
                    id: id.clone(),
                    downloaded: snapshot.downloaded,
                    total: snapshot.total,
                    fraction: snapshot.fraction(),
                },
            );
        })
    };

    let event = match store.install(&model, Digest::FromHub, Some(progress)).await {
        Ok(_) => ModelEvent::Finished {
            id,
            ok: true,
            cancelled: false,
            error: None,
        },
        Err(error) => ModelEvent::Finished {
            id,
            ok: false,
            cancelled: false,
            error: Some(download_error(error)),
        },
    };
    let _ = app.emit("model://event", event);
}

// ------------------------------------------------------------------ character

/// Update the character's inputs and emit `character://state` if the derived
/// state changed. Deduped so the 1-second run-status poll does not spam the UI.
async fn update_character(app: &AppHandle, mutate: impl FnOnce(&mut CharacterInputs)) {
    let state = app.state::<AppState>();
    let next = {
        let mut tracker = state.character.lock().await;
        mutate(&mut tracker.inputs);
        let next = character::derive(&tracker.inputs);
        if tracker.last == Some(next) {
            return;
        }
        tracker.last = Some(next);
        next
    };
    let _ = app.emit("character://state", next);
}

/// The current character state, for a UI that mounts after the last event fired.
#[tauri::command]
async fn character_state(state: tauri::State<'_, AppState>) -> Result<CharacterState, String> {
    let tracker = state.character.lock().await;
    Ok(tracker
        .last
        .unwrap_or_else(|| character::derive(&tracker.inputs)))
}

/// Set the Phase 5 hook signals from what the UI can observe today: `listening`
/// follows composer focus, `speaking` is set while a reply is rendered. The
/// voice pipeline replaces both sources without touching this seam.
#[tauri::command]
async fn set_character_signals(
    app: AppHandle,
    listening: Option<bool>,
    speaking: Option<bool>,
) -> Result<(), String> {
    update_character(&app, |inputs| {
        if let Some(listening) = listening {
            inputs.listening = listening;
        }
        if let Some(speaking) = speaking {
            inputs.speaking = speaking;
        }
    })
    .await;
    Ok(())
}

/// The active character: its id (for the dashboard picker) and its config URL
/// (for the widget's renderer).
#[derive(Serialize)]
struct CharacterSettings {
    active: String,
    config_url: String,
}

#[tauri::command]
fn character_settings(state: tauri::State<'_, AppState>) -> Result<CharacterSettings, String> {
    let settings = state
        .settings_store
        .load()
        .map_err(|error| error.to_string())?;
    let active = settings
        .character
        .as_ref()
        .map(CharacterId::as_str)
        .unwrap_or("hiyori")
        .to_string();
    Ok(CharacterSettings {
        config_url: format!("/characters/{active}/character.json"),
        active,
    })
}

/// Switch the character and reload the widget so its renderer remounts.
///
/// A character is an asset folder; if the id names a folder that is not
/// bundled, the widget's own load-failure path falls back to the placeholder
/// and logs why — the setting is not worth refusing.
#[tauri::command]
async fn set_character(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let id = CharacterId::new(id)?;
    state.update_settings(|settings| settings.character = Some(id))?;
    if let Some(window) = app.get_webview_window(WIDGET) {
        let _ = window.eval("window.location.reload()");
    }
    Ok(())
}

/// Store the UI's click-through mask for the cursor poller.
#[tauri::command]
fn set_hit_mask(state: tauri::State<'_, AppState>, mask: HitMask) -> Result<(), String> {
    if !mask.is_valid() || mask.cols.saturating_mul(mask.rows) > 1_000_000 {
        return Err("rejected an inconsistent hit mask".into());
    }
    tracing::debug!(cols = mask.cols, rows = mask.rows, "hit mask updated");
    if let Ok(mut slot) = state.hit_mask.lock() {
        *slot = Some(mask);
    }
    Ok(())
}

/// Surface a frontend error on stderr, where the developer is already watching.
///
/// The webview's console is awkward to reach in a transparent borderless widget;
/// this bridges the errors worth seeing (a failed character load, say) to the
/// same log the gateway and sidecar write to.
#[tauri::command]
fn log_ui_error(message: String) {
    tracing::error!(target: "ic_widget::ui", "{message}");
}

#[tauri::command]
async fn open_dashboard(app: AppHandle) -> Result<(), String> {
    show_dashboard(&app).map_err(|error| error.to_string())
}

// ------------------------------------------------------------- event pump

/// What the UI receives on `chat://event`. Mirrors `ChatEvent` in `ui/src/api.ts`.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ChatEvent {
    RunStatus {
        run_id: String,
        phase: String,
        failure_summary: Option<String>,
    },
    Gate {
        run_id: String,
        gate_ref: String,
        headline: String,
        body: String,
    },
    Activity {
        capability_id: String,
        status: String,
    },
    /// A connector's credential was refused and the run is **parked** waiting for
    /// a better one (Phase 8b).
    ///
    /// This is the event that must never become a spinner. The runtime's answer
    /// to a bad connector credential is not to fail the turn — it raises this and
    /// waits, indefinitely, for an answer that only the UI can provide.
    AuthGate {
        run_id: String,
        /// `auth_request_ref` — what the resolve and manual-token-submit routes
        /// want as their `gate_ref`.
        gate_ref: String,
        headline: String,
        body: String,
        /// Which connector is asking. The gateway **does not say** — this is
        /// inferred from the capability that failed just before the gate, which
        /// is the only place the answer exists.
        connector: Option<String>,
        /// Its auth provider, for the manual-token routes.
        provider: Option<String>,
    },
    StreamError {
        reason: String,
    },
}

/// Translate the gateway's projection stream into UI events.
///
/// The stream reconnects itself, so this task ends only when the thread changes,
/// the app exits, or the stream fails terminally.
async fn pump_events(app: AppHandle, client: GatewayClient, thread_id: ThreadId) {
    let mut stream = client.events(thread_id.clone());
    // The last capability the agent ran. An `auth_required` gate does not say
    // which connector wants the credential, so this is the only answer.
    let mut last_capability: Option<String> = None;
    while let Some(event) = stream.next().await {
        let translated = match event {
            Ok(GatewayEvent::ProjectionSnapshot(state) | GatewayEvent::ProjectionUpdate(state)) => {
                // A run's status is the only way to know a turn finished.
                for item in &state.items {
                    if let ProjectionItem::RunStatus(status) = item {
                        emit(
                            &app,
                            ChatEvent::RunStatus {
                                run_id: status.run_id.to_string(),
                                phase: phase_name(&status.status),
                                failure_summary: status.failure_summary.clone(),
                            },
                        );
                        update_character(&app, |inputs| inputs.run = Some(status.status.clone()))
                            .await;

                        // A user-initiated run just finished — maybe it taught
                        // something (Phase 7b). The watch fires once per run, only
                        // on an in-flight → completed edge, so the stream's
                        // repeats and snapshot replays cannot double-reflect.
                        let completed = app
                            .state::<AppState>()
                            .reflection_runs
                            .lock()
                            .await
                            .observe(status.run_id.as_ref(), &status.status);
                        if completed {
                            spawn_reflection(&app, thread_id.clone());
                        }
                    }
                }
                continue;
            }
            Ok(GatewayEvent::Gate(prompt)) => ChatEvent::Gate {
                run_id: prompt.turn_run_id.to_string(),
                gate_ref: prompt.gate_ref.to_string(),
                headline: prompt.headline,
                body: prompt.body,
            },
            Ok(GatewayEvent::CapabilityActivity(activity)) => {
                // Remember it: when an auth gate arrives moments later, this is
                // the *only* record of which connector raised it.
                last_capability = Some(activity.capability_id.clone());
                ChatEvent::Activity {
                    capability_id: activity.capability_id,
                    status: activity.status,
                }
            }
            Ok(GatewayEvent::AuthRequired(prompt)) => {
                // The prompt names no provider (verified — `headline` is a
                // generic "Authentication required"), so infer the connector from
                // the capability that just failed: `github.search_repositories`
                // → `github`.
                let connector = last_capability
                    .as_deref()
                    .and_then(|id| id.split('.').next())
                    .map(str::to_string);
                tracing::info!(
                    run = %prompt.turn_run_id,
                    connector = ?connector,
                    "a run is parked on an auth gate; the user must supply a credential"
                );
                update_character(&app, |inputs| inputs.auth_gate_pending = true).await;
                ChatEvent::AuthGate {
                    run_id: prompt.turn_run_id.to_string(),
                    gate_ref: prompt.auth_request_ref,
                    headline: prompt.headline,
                    body: prompt.body,
                    provider: prompt.provider.clone().or_else(|| connector.clone()),
                    connector,
                }
            }
            Ok(GatewayEvent::Error(error)) => ChatEvent::StreamError {
                reason: format!("The agent's event stream failed ({}).", error.kind),
            },
            // `keep_alive`, previews, and unknown events are not rendered.
            Ok(_) => continue,
            Err(error) => {
                tracing::warn!(%error, "the chat event stream ended");
                ChatEvent::StreamError {
                    reason: "Lost the agent's event stream.".into(),
                }
            }
        };
        emit(&app, translated);
    }
}

fn emit(app: &AppHandle, event: ChatEvent) {
    if let Err(error) = app.emit("chat://event", event) {
        tracing::warn!(%error, "could not deliver a chat event to the UI");
    }
}

/// The wire name for a run phase, matching `RunPhase` in `ui/src/api.ts`.
fn phase_name(phase: &RunPhase) -> String {
    match phase {
        RunPhase::Queued => "queued",
        RunPhase::Running => "running",
        RunPhase::CancelRequested => "cancel_requested",
        RunPhase::BlockedApproval => "blocked_approval",
        RunPhase::BlockedAuth => "blocked_auth",
        RunPhase::BlockedResource => "blocked_resource",
        RunPhase::BlockedDependentRun => "blocked_dependent_run",
        RunPhase::RecoveryRequired => "recovery_required",
        RunPhase::Completed => "completed",
        RunPhase::Cancelled => "cancelled",
        RunPhase::Failed => "failed",
        RunPhase::Killed => "killed",
        RunPhase::Other(_) => "other",
    }
    .to_string()
}

// ------------------------------------------------------ desktop interaction

/// How often the global cursor is sampled — matches the renderer's ~30 fps cap.
/// `set_ignore_cursor_events` is only touched on transitions.
const CURSOR_POLL: std::time::Duration = std::time::Duration::from_millis(33);
/// How often the foreground window is checked for fullscreen. Slow-moving state.
const FULLSCREEN_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// Window-local cursor position, logical pixels, emitted on `cursor://pos`.
#[derive(Serialize, Clone, Copy, PartialEq)]
struct CursorPos {
    x: f64,
    y: f64,
}

/// Drive click-through, cursor-following, and the fullscreen pause from one
/// global cursor poll.
///
/// The webview cannot own any of this: while `set_ignore_cursor_events(true)`
/// is set it receives no mouse events at all, so the decision to become
/// clickable again must come from outside it. The same poll feeds the
/// character's eye tracking (`cursor://pos`) and, at a slower cadence, pauses
/// animation while a fullscreen app is foreground (`character://active`) so the
/// idle character never competes with a game — or llama.cpp — for the GPU.
#[cfg(windows)]
fn spawn_interaction_watch(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut poll = tokio::time::interval(CURSOR_POLL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut ignoring = false;
        let mut animating = true;
        let mut last_cursor: Option<CursorPos> = None;
        let mut fullscreen_checked = std::time::Instant::now() - FULLSCREEN_POLL;

        loop {
            poll.tick().await;
            let Some(window) = app.get_webview_window(WIDGET) else {
                continue;
            };
            if !window.is_visible().unwrap_or(false) {
                continue;
            }

            if fullscreen_checked.elapsed() >= FULLSCREEN_POLL {
                fullscreen_checked = std::time::Instant::now();
                let active = !foreground_is_fullscreen();
                if active != animating {
                    animating = active;
                    let _ = app.emit("character://active", animating);
                }
            }

            let Some((cursor_x, cursor_y)) = cursor_position() else {
                continue;
            };
            let (Ok(origin), Ok(size), Ok(scale)) = (
                window.outer_position(),
                window.outer_size(),
                window.scale_factor(),
            ) else {
                continue;
            };
            let inside = cursor_x >= origin.x
                && cursor_y >= origin.y
                && cursor_x < origin.x + size.width as i32
                && cursor_y < origin.y + size.height as i32;
            let local = CursorPos {
                x: f64::from(cursor_x - origin.x) / scale,
                y: f64::from(cursor_y - origin.y) / scale,
            };

            // No mask yet (the UI is still booting): everything is clickable.
            // Failing interactive can cost a stray click on empty space; failing
            // click-through would strand a widget nothing can ever click again.
            let solid = inside
                && app
                    .state::<AppState>()
                    .hit_mask
                    .lock()
                    .map(|mask| {
                        mask.as_ref()
                            .is_none_or(|mask| mask.is_solid(local.x, local.y))
                    })
                    .unwrap_or(true);
            let want_ignore = inside && !solid;
            if want_ignore != ignoring && window.set_ignore_cursor_events(want_ignore).is_ok() {
                ignoring = want_ignore;
                tracing::debug!(click_through = ignoring, "cursor crossed a mask edge");
            }

            // Eye tracking: the character watches the cursor while it is over
            // the window (clickable or not). Deduped — a resting cursor at
            // 30 Hz would otherwise be pure IPC noise.
            if inside && animating && last_cursor != Some(local) {
                last_cursor = Some(local);
                let _ = app.emit("cursor://pos", local);
            }
        }
    });
}

/// The global cursor in screen (physical) coordinates.
#[cfg(windows)]
fn cursor_position() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut point = POINT::default();
    // SAFETY: `point` is a valid, writable POINT for the duration of the call.
    unsafe { GetCursorPos(&mut point) }.ok()?;
    Some((point.x, point.y))
}

/// Whether the foreground window covers its whole monitor.
///
/// The desktop shell itself (Progman/WorkerW) is fullscreen by geometry but
/// means "nothing is focused" — a character that paused whenever the user
/// clicked their wallpaper would look broken, so the shell classes are ignored.
#[cfg(windows)]
fn foreground_is_fullscreen() -> bool {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetWindowRect,
    };

    // SAFETY: no preconditions; may return a null handle.
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_invalid() {
        return false;
    }

    let mut class = [0u16; 32];
    // SAFETY: `class` is writable for its whole length.
    let written = unsafe { GetClassNameW(foreground, &mut class) }.max(0) as usize;
    let class = String::from_utf16_lossy(&class[..written.min(class.len())]);
    if class == "Progman" || class == "WorkerW" {
        return false;
    }

    let mut rect = RECT::default();
    // SAFETY: `rect` is a valid, writable RECT.
    if unsafe { GetWindowRect(foreground, &mut rect) }.is_err() {
        return false;
    }

    // SAFETY: the handle came from GetForegroundWindow above; a stale handle
    // yields a null monitor, which GetMonitorInfoW then rejects.
    let monitor = unsafe { MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is writable and `cbSize` is set as the API requires.
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return false;
    }
    let m = info.rcMonitor;
    rect.left <= m.left && rect.top <= m.top && rect.right >= m.right && rect.bottom >= m.bottom
}

// ----------------------------------------------------------------- windows

fn build_widget(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    WebviewWindowBuilder::new(app, WIDGET, WebviewUrl::App("index.html".into()))
        .title("IronClaw")
        .inner_size(380.0, 680.0)
        .min_inner_size(320.0, 420.0)
        .decorations(false)
        .transparent(true)
        // **The frame the user sees is this.** An undecorated, transparent window
        // still gets a DWM drop-shadow and rounded border on Windows 11 — drawn by
        // the compositor, not by us, so no amount of CSS removes it. It reads as a
        // window frame hanging in mid-air around a character that is supposed to be
        // standing on the desktop.
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false) // shown once its position is restored, to avoid a jump
        .build()
}

fn show_dashboard(app: &AppHandle) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window(DASHBOARD) {
        window.show()?;
        window.set_focus()?;
        return Ok(());
    }
    WebviewWindowBuilder::new(app, DASHBOARD, WebviewUrl::App("dashboard.html".into()))
        .title("IronClaw Dashboard")
        .inner_size(900.0, 700.0)
        .build()?;
    Ok(())
}

/// Display agent-authored markup on the canvas window, creating it on first use.
///
/// The markup is put into a `canvas://render` event that the trusted `canvas.html`
/// shell drops into a locked-down sandbox iframe (see the frontend). It is emitted
/// only to the canvas window — never broadcast — so no other webview ever receives
/// untrusted HTML. Returns an error string (surfaced to the agent as a recoverable
/// tool error) only if the window cannot be created; a created-and-shown window is
/// success even before the paint finishes.
fn show_canvas(app: &AppHandle, request: ic_widget::canvas::RenderRequest) -> Result<(), String> {
    let title = request
        .title
        .clone()
        .unwrap_or_else(|| "IronClaw Canvas".to_string());

    // Store before create/emit: the shell reads this on mount, which covers the
    // race where the window is still loading when the event fires.
    if let Ok(mut last) = app.state::<AppState>().last_canvas.lock() {
        *last = Some(request.clone());
    }

    let window = match app.get_webview_window(CANVAS) {
        Some(window) => window,
        None => WebviewWindowBuilder::new(app, CANVAS, WebviewUrl::App("canvas.html".into()))
            .title("IronClaw Canvas")
            .inner_size(720.0, 560.0)
            .build()
            .map_err(|error| format!("could not open the canvas window: {error}"))?,
    };
    let _ = window.set_title(&title);
    window
        .show()
        .map_err(|error| format!("could not show the canvas window: {error}"))?;

    // Emit for the already-open case; a first-open shell picks it up via
    // `canvas_content` on mount instead.
    window
        .emit("canvas://render", &request)
        .map_err(|error| format!("could not send markup to the canvas: {error}"))
}

/// The latest canvas render, for the shell to fetch on mount.
#[tauri::command]
async fn canvas_content(
    state: tauri::State<'_, AppState>,
) -> Result<Option<ic_widget::canvas::RenderRequest>, String> {
    Ok(state
        .last_canvas
        .lock()
        .map(|last| last.clone())
        .unwrap_or(None))
}

/// Toggle the widget. The global hotkey, the tray, and a second launch all land
/// here.
fn toggle_widget(app: &AppHandle) {
    let Some(window) = app.get_webview_window(WIDGET) else {
        return;
    };
    let result = if window.is_visible().unwrap_or(false) {
        window.hide()
    } else {
        window.show().and_then(|()| window.set_focus())
    };
    if let Err(error) = result {
        tracing::warn!(%error, "could not toggle the widget");
    }
}

/// The monitor arrangement, as the window layer sees it.
fn monitors(window: &WebviewWindow) -> Vec<MonitorInfo> {
    window
        .available_monitors()
        .unwrap_or_default()
        .into_iter()
        .map(|monitor| MonitorInfo {
            name: monitor.name().cloned(),
            x: monitor.position().x,
            y: monitor.position().y,
            width: monitor.size().width,
            height: monitor.size().height,
            scale: monitor.scale_factor(),
        })
        .collect()
}

/// Put the widget back where it was for this arrangement, or centre it.
fn restore_position(window: &WebviewWindow, state: &AppState) {
    let monitors = monitors(window);
    let layout = LayoutHash::of(&monitors);
    let saved = state
        .window_state
        .lock()
        .ok()
        .and_then(|guard| guard.position_for(&layout, &monitors));

    match saved {
        Some(position) => {
            let _ = window.set_position(tauri::PhysicalPosition::new(position.x, position.y));
        }
        // An arrangement we have not seen, or a saved point now offscreen.
        None => {
            let _ = window.center();
        }
    }
}

/// Remember where the widget is now, for this arrangement.
fn remember_position(window: &WebviewWindow, state: &AppState, position: WindowPosition) {
    let monitors = monitors(window);
    let layout = LayoutHash::of(&monitors);

    let snapshot = {
        let Ok(mut guard) = state.window_state.lock() else {
            return; // silent-ok: a poisoned lock costs one remembered position
        };
        guard.remember(&layout, position);
        guard.clone()
    };
    if let Err(error) = state.window_store.save(&snapshot) {
        tracing::warn!(%error, "could not save the widget position");
    }
}

// ------------------------------------------------------------------- boot

/// Where `ironclaw-reborn` lives: wherever `IRONCLAW_REBORN_BIN` says, beside us
/// once installed, or on `PATH` during development.
fn reborn_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("IRONCLAW_REBORN_BIN") {
        return PathBuf::from(path);
    }
    let name = if cfg!(windows) {
        "ironclaw-reborn.exe"
    } else {
        "ironclaw-reborn"
    };
    if let Ok(exe) = std::env::current_exe()
        && let Some(sibling) = exe.parent().map(|dir| dir.join(name))
        && sibling.exists()
    {
        return sibling;
    }
    PathBuf::from(name)
}

/// Where the gateway keeps its libSQL database and workspace.
fn reborn_home() -> Result<PathBuf, String> {
    dirs::data_local_dir()
        .map(|base| base.join("IronClaw Desktop").join("reborn"))
        .ok_or_else(|| "could not locate the local application data directory".to_string())
}

/// Where the local model store and the llama.cpp runtime live.
fn llama_root() -> Result<PathBuf, String> {
    dirs::data_local_dir()
        .map(|base| base.join("IronClaw Desktop").join("llama"))
        .ok_or_else(|| "could not locate the local application data directory".to_string())
}

/// Bring up a local model, if one is installed and usable.
///
/// Best-effort: any failure — no model, a suspect model, a model too big for
/// this machine, a llama.cpp download that did not complete — degrades to
/// `None`, and the gateway runs without local inference rather than failing to
/// start. Every failure is logged; none is swallowed silently.
///
/// The child `llama-server` is enlisted in `job` on every (re)spawn, so it dies
/// with the widget even under a hard kill — the same guarantee the gateway has.
async fn launch_local_model(
    job: Arc<ProcessJob>,
    settings: &ic_widget::settings::Settings,
) -> Option<LocalLlm> {
    let root = match llama_root() {
        Ok(root) => root,
        Err(error) => {
            tracing::warn!(%error, "no local model directory; running without local inference");
            return None;
        }
    };

    let downloader = match Downloader::new() {
        Ok(downloader) => downloader,
        Err(error) => {
            tracing::warn!(%error, "could not init the model downloader; skipping local inference");
            return None;
        }
    };

    let store = ModelStore::new(&root, downloader);
    let installed = match store.list().await {
        Ok(installed) => installed,
        Err(error) => {
            tracing::warn!(%error, "could not list installed models; skipping local inference");
            return None;
        }
    };

    // The model the user pinned, when it is still installed and usable. A pin
    // that no longer resolves (removed, or gone suspect since) falls back to the
    // old rule — first usable, deterministically ordered by the store — rather
    // than refusing to run any model at all.
    let pinned = settings.default_model.as_deref().and_then(|id| {
        let found = installed
            .iter()
            .find(|model| model.id.as_str() == id && model.is_loadable());
        if found.is_none() {
            tracing::warn!(
                model = id,
                "the pinned model is missing or suspect; using another"
            );
        }
        found.cloned()
    });
    let Some(model) = pinned.or_else(|| installed.into_iter().find(|model| model.is_loadable()))
    else {
        tracing::info!("no installed local model; the gateway will start without one");
        return None;
    };

    tracing::info!(model = %model.id, "bringing up the local model");
    let options = LocalLlmOptions {
        on_sidecar_spawn: Some(enlist_in_job(job)),
        cloud_fallback: cloud_fallback(settings),
        ..Default::default()
    };
    match LocalLlm::launch(&root, &model.id, options).await {
        Ok(llm) => {
            for warning in &llm.placement().warnings {
                tracing::warn!(model = %model.id, "{warning}");
            }
            tracing::info!(model = %model.id, "the local model is ready");
            Some(llm)
        }
        Err(error) => {
            tracing::warn!(model = %model.id, %error, "the local model did not start; running without it");
            None
        }
    }
}

/// The cloud endpoint the local model falls back to, or `None`.
///
/// This is how the v1 promise — "answer with a local GGUF model, with cloud
/// failover when a key is configured" — is kept without a core patch. The
/// gateway is told about exactly one provider (the proxy); the proxy itself
/// retries a failed completion against the cloud. Consequently **the cloud key
/// is never put in the gateway's environment**, which is strictly better than
/// the alternatives (`docs/desktop/llm-provider-selection.md`, option 2).
///
/// A configured fallback whose key is missing, or whose provider cannot be
/// spoken to in the OpenAI shape, degrades to `None` with a warning: running
/// locally with no safety net beats refusing to start.
fn cloud_fallback(settings: &ic_widget::settings::Settings) -> Option<CloudFallback> {
    let configured = settings.cloud_fallback.as_ref()?;
    let provider = match ic_widget::providers::find(&configured.id) {
        Ok(Some(provider)) => provider,
        Ok(None) => {
            tracing::warn!(
                provider = configured.id,
                "unknown fallback provider; ignoring it"
            );
            return None;
        }
        Err(error) => {
            tracing::warn!(%error, "could not read the provider catalog");
            return None;
        }
    };
    let base_url = provider.failover_base_url()?;
    let key = match SecretStore::new().provider_key(&provider) {
        Ok(Some(key)) => key,
        Ok(None) => {
            tracing::warn!(
                provider = configured.id,
                "the fallback provider has no stored key; running with no fallback"
            );
            return None;
        }
        Err(error) => {
            tracing::warn!(provider = configured.id, %error, "could not read the fallback key");
            return None;
        }
    };
    tracing::info!(
        provider = configured.id,
        "the local model has a cloud fallback"
    );
    Some(CloudFallback {
        base_url,
        api_key: key,
        model: configured
            .model
            .clone()
            .unwrap_or(provider.default_model.clone()),
    })
}

/// A spawn hook that puts `llama-server` in the widget's process job. Mirrors the
/// gateway's own containment: a child outside the job survives a hard kill.
fn enlist_in_job(job: Arc<ProcessJob>) -> SpawnHook {
    SpawnHook::new(move |child| {
        job.assign(child)
            .map_err(|error| std::io::Error::other(error.to_string()))
    })
}

/// The provider environment for a selection, plus the local model when one was
/// launched.
///
/// `Local` brings up the sidecar (best-effort; see [`launch_local_model`]).
/// `Cloud` reads the provider's key from the credential store and builds its
/// environment; a provider with no stored key yields an empty environment, so
/// the gateway starts but has no credentials — the dashboard shows that, rather
/// than the app refusing to launch.
async fn resolve_provider(
    job: Arc<ProcessJob>,
    selection: &ProviderSelection,
    settings: &ic_widget::settings::Settings,
) -> (Vec<(String, String)>, Option<LocalLlm>) {
    match selection {
        ProviderSelection::Local => {
            let local = launch_local_model(job, settings).await;
            let env = local
                .as_ref()
                .map(|llm| {
                    llm.env()
                        .vars()
                        .map(|(name, value)| (name.to_string(), value.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            (env, local)
        }
        ProviderSelection::Cloud {
            id,
            model,
            base_url,
        } => (
            cloud_provider_env(id, model.as_deref(), base_url.as_deref()),
            None,
        ),
    }
}

/// The environment that points the gateway at a cloud provider, or empty when it
/// cannot be built. Every empty-returning path is logged.
fn cloud_provider_env(
    id: &str,
    model: Option<&str>,
    base_url: Option<&str>,
) -> Vec<(String, String)> {
    let provider = match ic_widget::providers::find(id) {
        Ok(Some(provider)) => provider,
        Ok(None) => {
            tracing::warn!(
                provider = id,
                "unknown provider id; starting without credentials"
            );
            return Vec::new();
        }
        Err(error) => {
            tracing::warn!(%error, "could not read the provider catalog");
            return Vec::new();
        }
    };
    match SecretStore::new().provider_key(&provider) {
        Ok(Some(key)) => provider
            .llm_env(
                &key,
                model,
                base_url.or(provider.probe_base_url().as_deref()),
            )
            .unwrap_or_default(),
        Ok(None) => {
            tracing::warn!(
                provider = id,
                "no API key stored; starting without credentials"
            );
            Vec::new()
        }
        Err(error) => {
            tracing::warn!(provider = id, %error, "could not read the API key");
            Vec::new()
        }
    }
}

/// Start the gateway off the main thread on the saved provider selection.
///
/// A first boot runs migrations and installs bundled skills. Blocking the UI for
/// that would be a splash screen; a health badge is a better answer.
fn spawn_gateway(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let selection = match app.state::<AppState>().settings_store.load() {
            Ok(settings) => settings.active_provider,
            Err(error) => {
                tracing::warn!(%error, "could not read settings; defaulting to the local model");
                ProviderSelection::default()
            }
        };
        bring_up_gateway(app, selection).await;
    });
}

/// Bring the gateway up on `selection`, storing it into app state and mirroring
/// its health onto the UI. Shared by the first boot and by an
/// [`apply_provider`] restart.
///
/// The caller is responsible for tearing down any previous gateway and model
/// first; this replaces the stored `local_llm`, so a leftover would be dropped
/// here regardless, but the gateway must be stopped by the caller to free its
/// port before this reserves a new one.
async fn bring_up_gateway(app: AppHandle, selection: ProviderSelection) {
    let job = Arc::clone(&app.state::<AppState>().job);
    let settings = app
        .state::<AppState>()
        .settings_store
        .load()
        .unwrap_or_else(|error| {
            // silent-ok: unreadable settings mean defaults — no pinned model and
            // no fallback — which is the same as a fresh install.
            tracing::warn!(%error, "could not read settings; using defaults for the model");
            ic_widget::settings::Settings::default()
        });

    // The gateway reads `LLM_BASE_URL` once at startup and never re-reads it, so
    // the model must be up — and its proxy URL known — before the gateway spawns.
    let (llm_env, local) = resolve_provider(job.clone(), &selection, &settings).await;
    // Keep the model alive for the app's lifetime; its `Drop` stops the sidecar
    // and proxy on a graceful exit. Storing `None` drops any previous model.
    *app.state::<AppState>().local_llm.lock().await = local;

    // The browser sidecar must come up BEFORE the gateway, for two reasons: the
    // gateway scans its extension catalogue exactly once at boot, and the
    // manifest we write carries the sidecar's live port. A manifest written after
    // the gateway starts is invisible until the next restart.
    //
    // Best-effort: no browser on the machine means no browser tools, not a failed
    // launch. Storing `None` drops any previous sidecar (and its Chrome).
    if let Ok(home) = reborn_home() {
        // Surface each sensitive-fill approval request to the UI. The character
        // goes `concerned` alongside, the same as a tool gate.
        let sink: ic_widget::browser::ApprovalSink = {
            let handle = app.clone();
            Arc::new(move |request| {
                let _ = handle.emit("browser://approval", &request);
                let handle = handle.clone();
                tauri::async_runtime::spawn(async move {
                    update_character(&handle, |inputs| inputs.browser_approval_pending = true)
                        .await;
                });
            })
        };
        let sidecar = ic_widget::browser::start(job.clone(), &home, sink).await;
        *app.state::<AppState>().browser.lock().await = sidecar;

        // The canvas server is also before the gateway, for the same catalogue-scan
        // reason. Its sink shows (and creates on first use) the canvas window and
        // hands it the markup — the markup never crosses the gateway.
        let render_sink = {
            let handle = app.clone();
            Arc::new(CallbackSink(
                move |request: ic_widget::canvas::RenderRequest| show_canvas(&handle, request),
            ))
        };
        let canvas = ic_widget::canvas::start(&home, render_sink).await;
        *app.state::<AppState>().canvas.lock().await = canvas;
    }

    // Ambient mode is what switches the gateway's trigger poller on. It is read
    // here, at spawn, because the runtime reads its environment once and never
    // again — which is why the toggle restarts the gateway.
    let ambient_enabled = app
        .state::<AppState>()
        .settings_store
        .load()
        .map(|settings| settings.ambient_enabled)
        .unwrap_or(false); // silent-ok: unreadable settings mean no ambient, the safe side

    let started = async {
        let token = SecretStore::new()
            .gateway_token()
            .map_err(|error| error.to_string())?;
        let mut config = GatewayConfig::new(reborn_binary(), reborn_home()?, token)
            .map_err(|error| error.to_string())?;
        config.llm_env = llm_env;
        // Ambient's trigger poller plus, when a Google OAuth client is configured,
        // the environment `serve` needs to run the connector OAuth flow. The
        // redirect URI is built from our fixed callback port so it matches what
        // the user registered with Google.
        let mut extra_env = ambient_env(ambient_enabled);
        extra_env.extend(google_oauth_env(settings.google_oauth.callback_port));
        config.extra_env = extra_env;
        GatewaySupervisor::start(config, job)
            .await
            .map_err(|error| error.to_string())
    }
    .await;

    match started {
        Ok(gateway) => {
            tracing::info!(base_url = gateway.client().base_url(), "gateway is ready");

            // Publish the browser tools to the agent. This has to happen against
            // the *running* gateway and on *every* launch: activation is when the
            // gateway calls the sidecar's `tools/list`, and a restart republishes
            // only the bundled capability template, not the discovered tools.
            if app.state::<AppState>().browser.lock().await.is_some() {
                ic_widget::browser::register(gateway.client()).await;
            }
            if app.state::<AppState>().canvas.lock().await.is_some() {
                ic_widget::canvas::register(gateway.client()).await;
            }

            // Teach the agent its name and the user's. This has to run *after* the
            // gateway boots, because the runtime seeds the system-prompt file only
            // when it is missing — writing it first would rob the agent of the
            // runtime's own default instructions. The file is re-read every run, so
            // no restart is needed for it to take.
            if let Ok(home) = reborn_home()
                && let Ok(settings) = app.state::<AppState>().settings_store.load()
            {
                ic_widget::persona::apply(&home, &settings);
            }

            // Mirror every later transition onto the UI.
            let mut states = gateway.subscribe();
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                while states.changed().await.is_ok() {
                    let current = states.borrow_and_update().clone();
                    tracing::info!(?current, "gateway state changed");
                    let _ = handle.emit("gateway://state", &current);
                    update_character(&handle, |inputs| inputs.gateway = current.clone()).await;
                }
            });

            // Ambient mode watches *this* gateway, so it is (re)started with it. A
            // no-op when the toggle is off.
            let client = gateway.client().clone();

            // Store *before* emitting. A UI that has not subscribed yet will miss
            // the event and fall back to reading `gateway_state`, and that read
            // must already see `Ready` — otherwise the widget waits forever for an
            // event that has been and gone.
            *app.state::<AppState>().gateway.lock().await = Some(gateway);
            let _ = app.emit("gateway://state", GatewayState::Ready);
            update_character(&app, |inputs| inputs.gateway = GatewayState::Ready).await;

            start_ambient(&app, client).await;
        }
        Err(reason) => {
            tracing::error!(%reason, "the gateway did not start");
            update_character(&app, |inputs| {
                inputs.gateway = GatewayState::Unhealthy {
                    reason: reason.clone(),
                }
            })
            .await;
            let _ = app.emit("gateway://state", GatewayState::Unhealthy { reason });
        }
    }
}

// ---------------------------------------------------------------- ambient

/// The append-only record of every time the character spoke first.
fn ambient_log_path() -> Result<PathBuf, String> {
    data_root()
        .map(|base| base.join("ambient-log.jsonl"))
        .ok_or_else(|| "could not locate the local application data directory".to_string())
}

/// The environment the gateway needs for ambient mode.
///
/// Exactly one variable, and it is the whole reason a toggle restarts the gateway:
/// `serve` leaves its trigger poller **off** by default, so a scheduled automation
/// is listed by `GET /automations` and *never fires*. The runtime reads its
/// environment once at boot, so this cannot be turned on under a running gateway.
///
/// Tying it to the ambient toggle is deliberate. `builtin__trigger_create` runs with
/// no approval prompt (the runtime never enforces `default_permission` — Phase 4),
/// so an agent talked into arming a recurring prompt would otherwise have a
/// heartbeat the user never granted. Ambient off ⇒ no unprompted run exists.
fn ambient_env(enabled: bool) -> Vec<(String, String)> {
    match enabled {
        true => vec![("IRONCLAW_TRIGGER_POLLER_ENABLED".into(), "true".into())],
        false => Vec::new(),
    }
}

/// The environment that lets `serve` run the Google OAuth flow (Phase 8b.1), or
/// empty when no client is configured.
///
/// **This one reaches the gateway on purpose**, unlike a cloud provider key. The
/// `ic_llama` proxy deliberately keeps cloud keys out of `serve`'s environment
/// because it owns the retry; but the Google OAuth token exchange is `serve`'s
/// own — it holds the PKCE verifier — so `serve` genuinely needs the client
/// secret. `serve` reads these once at boot (`resolve_google_oauth_config`,
/// `ironclaw_reborn_cli`), so configuring a client restarts the gateway. The
/// redirect URI is built from our fixed callback `port` and must match what the
/// user registered with Google.
fn google_oauth_env(port: u16) -> Vec<(String, String)> {
    match SecretStore::new().google_oauth() {
        Ok(Some(client)) => vec![
            ("IRONCLAW_REBORN_GOOGLE_CLIENT_ID".into(), client.client_id),
            (
                "IRONCLAW_REBORN_GOOGLE_CLIENT_SECRET".into(),
                client.client_secret,
            ),
            (
                "IRONCLAW_REBORN_GOOGLE_OAUTH_REDIRECT_URI".into(),
                ic_widget::oauth_callback::redirect_uri(port),
            ),
        ],
        Ok(None) => Vec::new(),
        Err(error) => {
            // silent-ok: an unreadable client just means connector OAuth is
            // unavailable this launch, the same as never configuring one.
            tracing::warn!(%error, "could not read the Google OAuth client; connector OAuth unavailable");
            Vec::new()
        }
    }
}

/// Bring ambient mode up against a ready gateway. A no-op when it is switched off.
///
/// Called on every gateway start (first boot and every provider/ambient restart), so
/// the watcher is always bound to the gateway that is actually running.
async fn start_ambient(app: &AppHandle, client: GatewayClient) {
    let state = app.state::<AppState>();
    let Ok(settings) = state.settings_store.load() else {
        return;
    };
    if !settings.ambient_enabled {
        return;
    }

    let log = match ambient_log_path().and_then(|path| {
        ic_widget::ambient::log::SurfacingLog::open(path).map_err(|error| error.to_string())
    }) {
        Ok(log) => log,
        Err(error) => {
            // The log is the rate limiter's memory. Without it the character could
            // talk without a cap, so ambient mode stays off rather than uncapped.
            tracing::error!(%error, "could not open the ambient log; ambient mode stays off");
            return;
        }
    };

    // Settings are re-read on every check, so changing the cap or the quiet hours
    // takes effect on the next tick rather than the next launch.
    let store = state.settings_store.clone();
    let config: ic_widget::ambient::ConfigFn = Arc::new(move || match store.load() {
        Ok(settings) => AmbientConfig {
            enabled: settings.ambient_enabled,
            settings: settings.ambient,
        },
        Err(error) => {
            // Unreadable settings must not mean "no limits". Fail closed.
            tracing::warn!(%error, "could not read the ambient settings; staying quiet");
            AmbientConfig {
                enabled: false,
                settings: AmbientSettings::default(),
            }
        }
    });

    let sink: ic_widget::ambient::SuggestionSink = {
        let handle = app.clone();
        Arc::new(move |suggestion: Suggestion| {
            let _ = handle.emit("ambient://suggestion", &suggestion);
            let handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                update_character(&handle, |inputs| inputs.suggestion_pending = true).await;
            });
        })
    };

    let service = Arc::new(AmbientService::new(client.clone(), config, sink, log));

    // The ambient thread, reused across launches (threads outlive the gateway).
    let saved = settings.ambient.thread_id.clone();
    if let Some(thread_id) = ic_widget::ambient::ensure_thread(&client, saved.as_deref()).await {
        if saved.as_deref() != Some(thread_id.as_str()) {
            let id = thread_id.to_string();
            let _ = state.update_settings(|settings| settings.ambient.thread_id = Some(id));
        }
        *state.ambient_thread.lock().await = Some(thread_id);
    }

    let task = tokio::spawn(ic_widget::ambient::automations::watch(Arc::clone(&service)));
    if let Some(previous) = state.ambient_task.lock().await.replace(task) {
        previous.abort();
    }
    let watch_task = spawn_watchers(app, Arc::clone(&service));
    if let Some(previous) = state.watcher_task.lock().await.replace(watch_task) {
        previous.abort();
    }
    *state.ambient.lock().await = Some(service);
    tracing::info!("ambient mode is on: the character may speak first");
}

/// Fire one reflection turn for a just-completed chat run (Phase 7b).
///
/// Both toggles are read *at fire time*, so flipping either takes effect on the
/// next completed run rather than the next launch. A `tokio` spawn, not a Tauri
/// one, for the same reason as the pump: the reflection turn drives an
/// `EventStream`, which is `Send` but not `Sync`. Always called from inside the
/// pump task, so the runtime is present.
fn spawn_reflection(app: &AppHandle, chat_thread: ThreadId) {
    let app = app.clone();
    tokio::spawn(async move {
        let state = app.state::<AppState>();
        let Ok(settings) = state.settings_store.load() else {
            return; // silent-ok: unreadable settings mean no reflection, the safe side
        };
        if !settings.ambient_enabled || !settings.reflection_enabled {
            return;
        }
        let Some(service) = state.ambient.lock().await.clone() else {
            return;
        };
        let Some(ambient_thread) = state.ambient_thread.lock().await.clone() else {
            return;
        };
        let Ok(skills_root) = skills_root() else {
            return; // silent-ok: no data dir means nowhere to check or install skills
        };
        let outcome = ic_widget::ambient::reflection::reflect(
            &service,
            &ambient_thread,
            &chat_thread,
            &skills_root,
            ic_widget::ambient::reflection::DEFAULT_MAX_LEARNED,
        )
        .await;
        tracing::info!(?outcome, "the reflection turn finished");
    });
}

/// Where the gateway reads user skills from: plain files under its home.
/// Verified by the `skill_install` gate — a directory here with a `SKILL.md` is
/// listed, activatable, and fully injected on activation (the trusted tier).
fn skills_root() -> Result<PathBuf, String> {
    reborn_home().map(|home| home.join("local-dev").join("skills"))
}

/// Wind ambient mode down: stop watching, and take any unanswered suggestion off
/// the character's face.
async fn stop_ambient(app: &AppHandle) {
    let state = app.state::<AppState>();
    if let Some(task) = state.ambient_task.lock().await.take() {
        task.abort();
    }
    if let Some(task) = state.watcher_task.lock().await.take() {
        // Aborting drops the task's locals, including the `notify` watcher —
        // ambient off means not even a folder event is received.
        task.abort();
    }
    *state.ambient.lock().await = None;
    *state.ambient_thread.lock().await = None;
    update_character(app, |inputs| inputs.suggestion_pending = false).await;
}

// ---------------------------------------------------------------- watchers

/// How often the watcher loop samples its signals (Phase 7d). Slow-moving
/// state: a window switch or a folder drop is just as much news at 3 s.
const WATCH_POLL: std::time::Duration = std::time::Duration::from_secs(3);

/// Run the ambient watchers against a live gateway.
///
/// Settings are re-read every cycle, so rule edits and kind toggles take effect
/// on the next sample — only the ambient master switch needs a restart, and
/// that is the gateway's constraint, not this loop's. A `tokio` spawn (not a
/// Tauri one) because a firing drives an `EventStream`, `Send` but not `Sync`.
fn spawn_watchers(app: &AppHandle, service: Arc<AmbientService>) -> tokio::task::JoinHandle<()> {
    use ic_widget::ambient::watch::{Signal, WatchEngine, run_rule_fire};

    let app = app.clone();
    tokio::spawn(async move {
        let state = app.state::<AppState>();
        let mut engine = WatchEngine::new();
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();
        // Held for its Drop: reassigning (or ending the task) unwatches.
        let mut _folder_watcher: Option<notify::RecommendedWatcher> = None;
        let mut watched_paths: Vec<String> = Vec::new();
        let mut poll = tokio::time::interval(WATCH_POLL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            poll.tick().await;
            let Ok(settings) = state.settings_store.load() else {
                continue; // silent-ok: unreadable settings mean no watching, the safe side
            };
            if !settings.ambient_enabled {
                continue;
            }
            let watchers = settings.watchers;

            // Reconcile the folder watcher with the rules as they are now.
            let folders: Vec<String> = if watchers.folders_enabled {
                watchers
                    .rules
                    .iter()
                    .filter(|rule| rule.enabled)
                    .filter_map(|rule| match &rule.trigger {
                        ic_widget::settings::WatchTrigger::FolderChanged { path } => {
                            Some(path.clone())
                        }
                        _ => None,
                    })
                    .collect()
            } else {
                Vec::new()
            };
            if folders != watched_paths {
                _folder_watcher = build_folder_watcher(&folders, events_tx.clone());
                watched_paths = folders;
            }

            let now = chrono::Local::now();
            let mut firings = Vec::new();
            if watchers.foreground_enabled {
                let title = foreground_title();
                firings.extend(engine.observe(&watchers, &Signal::Foreground(title), now));
            }
            while let Ok(path) = events_rx.try_recv() {
                firings.extend(engine.observe(&watchers, &Signal::FolderEvent(path), now));
            }
            if watchers.time_enabled {
                firings.extend(engine.observe(&watchers, &Signal::Tick, now));
            }

            for firing in firings {
                let service = Arc::clone(&service);
                tokio::spawn(async move {
                    run_rule_fire(&service, &firing).await;
                });
            }
        }
    })
}

/// One recursive watcher over `paths`, or `None` when there are none (or the
/// platform refuses). A path that cannot be watched is a warning, not a veto —
/// one bad rule must not silence the others.
fn build_folder_watcher(
    paths: &[String],
    events: tokio::sync::mpsc::UnboundedSender<PathBuf>,
) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher as _;

    if paths.is_empty() {
        return None;
    }
    let mut watcher =
        match notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
            match event {
                Ok(event) => {
                    for path in event.paths {
                        let _ = events.send(path);
                    }
                }
                Err(error) => tracing::debug!(%error, "a folder watch event was unreadable"),
            }
        }) {
            Ok(watcher) => watcher,
            Err(error) => {
                tracing::warn!(%error, "could not create the folder watcher");
                return None;
            }
        };
    for path in paths {
        if let Err(error) = watcher.watch(Path::new(path), notify::RecursiveMode::Recursive) {
            tracing::warn!(%error, path, "could not watch a folder");
        }
    }
    Some(watcher)
}

/// The foreground window's title, or empty when there is none. The only thing
/// ever read is the *title* — no screen content, nothing leaves the machine.
#[cfg(windows)]
fn foreground_title() -> String {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW};

    // SAFETY: no preconditions; may return a null handle.
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_invalid() {
        return String::new();
    }
    let mut title = [0u16; 512];
    // SAFETY: `title` is writable for its whole length.
    let written = unsafe { GetWindowTextW(foreground, &mut title) }.max(0) as usize;
    String::from_utf16_lossy(&title[..written.min(title.len())])
}

/// Windows is the primary target; elsewhere the signal simply never fires.
#[cfg(not(windows))]
fn foreground_title() -> String {
    String::new()
}

/// What the dashboard shows for the watchers (Phase 7d).
#[tauri::command]
async fn watchers_status(
    state: tauri::State<'_, AppState>,
) -> Result<ic_widget::settings::WatcherSettings, String> {
    Ok(state
        .settings_store
        .load()
        .map_err(|error| error.to_string())?
        .watchers)
}

/// Switch the signal kinds on or off. Takes effect on the next sample.
#[tauri::command]
async fn set_watcher_kinds(
    state: tauri::State<'_, AppState>,
    foreground: bool,
    folders: bool,
    time: bool,
) -> Result<(), String> {
    state.update_settings(|settings| {
        settings.watchers.foreground_enabled = foreground;
        settings.watchers.folders_enabled = folders;
        settings.watchers.time_enabled = time;
    })?;
    Ok(())
}

/// Replace the rule list. Rules are the user's own configuration — editable
/// and deletable, unlike the logs, which record what was *shown* and stay.
#[tauri::command]
async fn set_watch_rules(
    state: tauri::State<'_, AppState>,
    rules: Vec<ic_widget::settings::WatchRule>,
) -> Result<(), String> {
    for rule in &rules {
        validate_watch_rule(rule)?;
    }
    state.update_settings(|settings| settings.watchers.rules = rules.clone())?;
    Ok(())
}

/// Refuse a rule that could never fire sensibly, with a reason the panel shows.
fn validate_watch_rule(rule: &ic_widget::settings::WatchRule) -> Result<(), String> {
    use ic_widget::settings::WatchTrigger;

    if rule.id.trim().is_empty() {
        return Err("a rule needs an id".to_string());
    }
    if rule.prompt.trim().is_empty() {
        return Err("a rule needs a prompt — the thing to ask the agent".to_string());
    }
    match &rule.trigger {
        WatchTrigger::ForegroundApp { title_contains } if title_contains.trim().is_empty() => {
            Err("a window rule needs text to look for in the title".to_string())
        }
        WatchTrigger::FolderChanged { path } if !Path::new(path).is_dir() => {
            Err(format!("{path} is not a folder"))
        }
        WatchTrigger::TimeOfDay { hour, minute } if *hour > 23 || *minute > 59 => {
            Err("a time rule needs a valid hour and minute".to_string())
        }
        _ => Ok(()),
    }
}

/// What the dashboard shows for ambient mode.
#[derive(Serialize)]
struct AmbientStatus {
    enabled: bool,
    reflection_enabled: bool,
    /// Whether the watcher is actually running against a live gateway.
    running: bool,
    max_per_hour: u32,
    quiet_start: Option<u32>,
    quiet_end: Option<u32>,
}

#[tauri::command]
async fn ambient_status(state: tauri::State<'_, AppState>) -> Result<AmbientStatus, String> {
    let settings = state.settings_store.load().map_err(|e| e.to_string())?;
    Ok(AmbientStatus {
        enabled: settings.ambient_enabled,
        reflection_enabled: settings.reflection_enabled,
        running: state.ambient.lock().await.is_some(),
        max_per_hour: settings.ambient.max_per_hour,
        quiet_start: settings.ambient.quiet_hours.map(|quiet| quiet.start_hour),
        quiet_end: settings.ambient.quiet_hours.map(|quiet| quiet.end_hour),
    })
}

/// Turn ambient mode on or off.
///
/// **This restarts the gateway**, because the trigger poller is an environment
/// switch the runtime reads once at boot (see [`ambient_env`]). The provider panel
/// already restarts for the same reason, so the machinery is shared.
#[tauri::command]
async fn set_ambient_enabled(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    let settings = state.update_settings(|settings| settings.ambient_enabled = enabled)?;
    stop_ambient(&app).await;
    restart_gateway(app, settings.active_provider).await;
    Ok(())
}

/// Change the guardrails. Takes effect on the next tick — no restart: only the
/// poller is an environment switch, and the caps are read fresh on every check.
#[tauri::command]
async fn set_ambient_guardrails(
    state: tauri::State<'_, AppState>,
    max_per_hour: u32,
    quiet_start: Option<u32>,
    quiet_end: Option<u32>,
) -> Result<(), String> {
    if max_per_hour == 0 || max_per_hour > 20 {
        return Err("the hourly cap must be between 1 and 20".into());
    }
    let quiet = match (quiet_start, quiet_end) {
        (Some(start), Some(end)) => {
            if start > 23 || end > 23 {
                return Err("quiet hours must be between 0 and 23".into());
            }
            Some(QuietHours {
                start_hour: start,
                end_hour: end,
            })
        }
        _ => None,
    };
    state.update_settings(|settings| {
        settings.ambient.max_per_hour = max_per_hour;
        settings.ambient.quiet_hours = quiet;
    })?;
    Ok(())
}

/// Turn the reflection pass on or off (Phase 7b). No restart: the toggle is
/// read when a run completes, unlike the ambient master switch, which has to
/// replace the gateway for its environment variable.
#[tauri::command]
async fn set_reflection_enabled(
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    state.update_settings(|settings| settings.reflection_enabled = enabled)?;
    Ok(())
}

/// What the UI hears after an approved draft's install attempt.
#[derive(Serialize, Clone)]
struct SkillInstallResult {
    ok: bool,
    name: Option<String>,
    error: Option<String>,
}

// ------------------------------------------------------------ skill import

/// The record of every import consent card and its answer (Phase 7c). Its own
/// file, deliberately not the ambient log: that one is the guardrail's rate
/// memory, and a solicited import must never spend (or later replay into) an
/// unsolicited-surfacing slot.
fn import_log_path() -> Result<PathBuf, String> {
    data_root()
        .map(|base| base.join("skill-import-log.jsonl"))
        .ok_or_else(|| "could not locate the local application data directory".to_string())
}

/// Append one event to the import log. Best-effort — an import the user can
/// see and answer beats one refused because a log line failed — but loud,
/// because the log is the audit trail of what was consented to.
fn record_import_event(event: ic_widget::ambient::log::LogEvent) {
    let outcome = import_log_path().and_then(|path| {
        let mut log =
            ic_widget::ambient::log::SurfacingLog::open(path).map_err(|error| error.to_string())?;
        log.record(ic_widget::ambient::log::LogEntry {
            at: chrono::Utc::now(),
            event,
        })
        .map_err(|error| error.to_string())
    });
    if let Err(error) = outcome {
        tracing::error!(%error, "could not record a skill-import event");
    }
}

/// List the user's installed skills for the dashboard (Phase 8c).
///
/// Reads the widget-owned skills root — the same directory 7b/7c install into,
/// not the gateway's private libSQL store — so it needs no route and no LLM
/// turn. Bundled runtime skills live elsewhere and are deliberately not listed.
#[tauri::command]
async fn list_installed_skills() -> Result<Vec<ic_widget::skills::InstalledSkill>, String> {
    let root = skills_root()?;
    ic_widget::skills::list(&root)
}

/// Remove one installed skill by name (Phase 8c).
///
/// Symmetric with the 7b/7c install: it deletes a directory under the same
/// widget-owned root. A skill is user-authored procedure, not LLM data, so this
/// is a permitted user-initiated removal — the UI confirms first.
#[tauri::command]
async fn remove_installed_skill(name: String) -> Result<(), String> {
    let root = skills_root()?;
    ic_widget::skills::remove(&root, &name)
}

/// Read a skill folder and return everything the review needs. Pure — nothing
/// is stored and nothing can install from here.
#[tauri::command]
async fn preview_skill_import(
    path: String,
) -> Result<ic_widget::skill_import::ImportPreview, String> {
    ic_widget::skill_import::preview(Path::new(&path))
}

/// Put a reviewed import on the bubble as a consent card (Phase 7c).
///
/// The folder is re-read here — the preview the dashboard showed is advisory;
/// what the card carries is what this call read, and what an approval installs
/// is exactly the card's text. Works with ambient off: an import is solicited,
/// so it neither needs the ambient service nor touches the guardrail.
#[tauri::command]
async fn request_skill_import(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    let folder = PathBuf::from(&path);
    let preview = ic_widget::skill_import::preview(&folder)?;

    let suggestion = Suggestion {
        id: uuid::Uuid::new_v4().to_string(),
        kind: SuggestionKind::SkillImport,
        key: format!("skill-import:{}", preview.name),
        source: format!("import:{}", preview.name),
        headline: format!(
            "Install the skill \u{201c}{}\u{201d} from this folder?",
            preview.name
        ),
        body: preview.skill_md.clone(),
        thread_id: None,
    };
    record_import_event(ic_widget::ambient::log::LogEvent::Surfaced {
        id: suggestion.id.clone(),
        key: suggestion.key.clone(),
        source: suggestion.source.clone(),
        headline: suggestion.headline.clone(),
    });
    *state.pending_import.lock().await = Some(PendingImport {
        id: suggestion.id.clone(),
        folder: Some(folder),
        name: preview.name,
        skill_md: preview.skill_md,
        overwrite: false,
    });
    let _ = app.emit("ambient://suggestion", &suggestion);
    update_character(&app, |inputs| inputs.suggestion_pending = true).await;
    Ok(())
}

/// One skill in a cloned repo, as shown in the dashboard review (Phase 8e).
#[derive(Serialize)]
struct RepoSkillReview {
    /// The skill's own name in the repo.
    name: String,
    /// The namespaced name it installs as (`<repo-slug>-<name>`).
    install_name: String,
    description: String,
    /// The folder within the repo, for display.
    rel_dir: String,
    /// The full namespaced SKILL.md text — rendered as PLAIN TEXT in the review,
    /// never markdown (untrusted third-party content).
    skill_md: String,
    /// Bundle files that ride along.
    files: Vec<ic_widget::skill_import::ImportFile>,
    /// Bundle content this app has no lane for and will never run (`hooks/`
    /// first). A skill written for another host is half-inert here, and the user
    /// must not find that out by wondering why nothing fires.
    inert: Vec<ic_widget::skill_import::InertLane>,
    /// What activating this skill costs the model's context, priced the way the
    /// runtime charges it.
    cost: ic_widget::skill_import::ContextCost,
    /// Hidden-character warnings (zero-width / bidi) found in the text, if any.
    warnings: Vec<String>,
    /// Whether a skill of this namespaced name is already installed (an update).
    installed: bool,
    /// When updating, the line diff installed → incoming, so only what changed is
    /// re-reviewed.
    diff: Option<Vec<ic_widget::git_import::DiffLine>>,
}

/// The result of cloning and scanning a skills repo for review.
#[derive(Serialize)]
struct RepoPreview {
    slug: String,
    url: String,
    skills: Vec<RepoSkillReview>,
    /// `SKILL.md` folders the repo ships that cannot be imported, with reasons.
    /// Reported rather than hidden: a repo listing 17 of its 18 skills with no
    /// explanation reads as a repo with 17 skills.
    rejected: Vec<ic_widget::git_import::RejectedSkill>,
}

/// Clone a git repo of skills and return each skill for review (Phase 8e).
///
/// The clone is depth-1, no-submodules, size- and time-capped, and symlink-free
/// (all enforced in `git_import`). It is kept alive in app state so an approved
/// install can copy its bundle; a new clone replaces (and deletes) the old one.
/// Nothing installs here — this is a pure read, the same shape as the 7c folder
/// preview.
#[tauri::command]
async fn preview_repo_skills(
    state: tauri::State<'_, AppState>,
    url: String,
) -> Result<RepoPreview, String> {
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("enter a git repository URL".to_string());
    }
    let temp =
        tempfile::tempdir().map_err(|error| format!("could not create a temp dir: {error}"))?;
    let into = temp.path().join("clone");
    let import = ic_widget::git_import::clone_and_scan(
        url,
        into,
        ic_widget::git_import::MAX_REPO_BYTES,
        ic_widget::git_import::CLONE_TIMEOUT,
    )
    .await?;

    // Build the review (installed?/diff/warnings) before handing the clone to state.
    let root = skills_root().ok();
    let mut skills = Vec::new();
    for skill in &import.skills {
        let existing = root.as_ref().and_then(|root| {
            std::fs::read_to_string(root.join(&skill.install_name).join("SKILL.md")).ok()
        });
        let (installed, diff) = match existing {
            Some(old) => (
                true,
                Some(ic_widget::git_import::diff_lines(&old, &skill.skill_md)),
            ),
            None => (false, None),
        };
        skills.push(RepoSkillReview {
            name: skill.name.clone(),
            install_name: skill.install_name.clone(),
            description: skill.description.clone(),
            rel_dir: skill.rel_dir.clone(),
            skill_md: skill.skill_md.clone(),
            inert: ic_widget::skill_import::inert_lanes(&skill.files),
            cost: ic_widget::skill_import::context_cost(&skill.skill_md),
            files: skill.files.clone(),
            warnings: ic_widget::git_import::suspicious_chars(&skill.skill_md),
            installed,
            diff,
        });
    }
    let preview = RepoPreview {
        slug: import.slug.clone(),
        url: import.url.clone(),
        skills,
        rejected: import.rejected.clone(),
    };
    *state.repo_clone.lock().await = Some(RepoClone { _dir: temp, import });
    Ok(preview)
}

/// Put one reviewed repo skill on the bubble as a red consent card (Phase 8e).
///
/// Reuses the 7c consent path exactly: an approval installs the reviewed text
/// verbatim through `respond_suggestion`. One skill at a time — never a bulk
/// silent install. An update (the namespaced name already exists) is flagged so
/// the install replaces rather than refuses.
#[tauri::command]
async fn request_repo_skill(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    install_name: String,
) -> Result<(), String> {
    let (folder, skill_md, name) = {
        let guard = state.repo_clone.lock().await;
        let clone = guard
            .as_ref()
            .ok_or("no repo has been cloned; review a repo first")?;
        let skill = clone
            .import
            .skills
            .iter()
            .find(|skill| skill.install_name == install_name)
            .ok_or_else(|| format!("no skill named {install_name} in the cloned repo"))?;
        (
            skill.folder.clone(),
            skill.skill_md.clone(),
            skill.install_name.clone(),
        )
    };
    let overwrite = skills_root()
        .map(|root| root.join(&name).exists())
        .unwrap_or(false);

    let suggestion = Suggestion {
        id: uuid::Uuid::new_v4().to_string(),
        kind: SuggestionKind::SkillImport,
        key: format!("skill-import:{name}"),
        source: format!("import:{name}"),
        headline: format!(
            "{} the skill \u{201c}{name}\u{201d}?",
            if overwrite { "Update" } else { "Install" }
        ),
        body: skill_md.clone(),
        thread_id: None,
    };
    record_import_event(ic_widget::ambient::log::LogEvent::Surfaced {
        id: suggestion.id.clone(),
        key: suggestion.key.clone(),
        source: suggestion.source.clone(),
        headline: suggestion.headline.clone(),
    });
    *state.pending_import.lock().await = Some(PendingImport {
        id: suggestion.id.clone(),
        folder: Some(folder),
        name,
        skill_md,
        overwrite,
    });
    let _ = app.emit("ambient://suggestion", &suggestion);
    update_character(&app, |inputs| inputs.suggestion_pending = true).await;
    Ok(())
}

/// What a "study this repo" run produced.
#[derive(Serialize)]
struct StudyResult {
    /// The repo slug studied.
    slug: String,
    /// The files the model was actually shown, so the user can judge the draft
    /// by what informed it. A study that read three files is a study worth
    /// distrusting, and hiding that would be the dishonest part.
    files_read: Vec<String>,
    /// Files left unread by the caps.
    skipped: usize,
    /// The repo's tool surface, as observed from its manifests. Descriptive: the
    /// widget cannot register an arbitrary repo as a connector, so this names
    /// what a user could wire up themselves in the Connectors panel.
    tool_surface: Vec<String>,
    /// The drafted skill's name, when the study produced one.
    drafted: Option<String>,
    /// Why no draft, when there is none.
    note: Option<String>,
}

/// Study a git repo and, if it teaches a procedure, draft a skill from it (8e).
///
/// The clone is the same guarded one the skill import uses; the reading list is
/// **bounded** (a dozen files, README and manifests first) because a small local
/// model cannot read a repository — the Phase 7b model-quality hint applies and
/// the panel surfaces it. The turn runs on its own fresh thread, not the ambient
/// thread and not the user's chat: a study is *solicited*, so it must work with
/// ambient off and must never spend a guardrail slot.
///
/// Nothing installs here. A draft lands on the bubble as the same red consent
/// card as every other skill, and only a yes writes it.
#[tauri::command]
async fn study_repo(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    url: String,
) -> Result<StudyResult, String> {
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("enter a git repository URL".to_string());
    }
    let client = state.client().await?;

    let temp =
        tempfile::tempdir().map_err(|error| format!("could not create a temp dir: {error}"))?;
    let study = ic_widget::git_import::clone_and_study(
        url,
        temp.path().join("clone"),
        ic_widget::git_import::MAX_REPO_BYTES,
        ic_widget::git_import::CLONE_TIMEOUT,
    )
    .await?;
    let files_read: Vec<String> = study
        .files
        .iter()
        .map(|file| file.rel_path.clone())
        .collect();
    let mut result = StudyResult {
        slug: study.slug.clone(),
        files_read,
        skipped: study.skipped,
        tool_surface: study.tool_surface.clone(),
        drafted: None,
        note: None,
    };

    // A fresh thread per study: its transcript is exactly this repo's reading,
    // so an accepted draft can be traced to what produced it.
    let thread_id = client
        .create_thread()
        .await
        .map_err(|error| format!("could not open a thread for the study: {error}"))?;
    let prompt = ic_widget::git_import::study_prompt(&study);
    let reply = match ic_widget::voice::drive_turn(&client, &thread_id, &prompt).await {
        ic_widget::voice::TurnResult::Reply(text) => text,
        _ => {
            result.note = Some("the agent did not answer the study".to_string());
            return Ok(result);
        }
    };

    let Some(draft) = ic_widget::ambient::reflection::parse_draft(&reply) else {
        result.note =
            Some("the agent found no reusable procedure in this repo to keep as a skill".into());
        return Ok(result);
    };

    let overwrite = skills_root()
        .map(|root| root.join(&draft.name).exists())
        .unwrap_or(false);
    let suggestion = Suggestion {
        id: uuid::Uuid::new_v4().to_string(),
        kind: SuggestionKind::SkillImport,
        key: format!("skill-import:{}", draft.name),
        source: format!("study:{}", draft.name),
        headline: format!(
            "{} \u{201c}{}\u{201d}, learned from {}?",
            if overwrite { "Update" } else { "Install" },
            draft.name,
            study.slug
        ),
        body: draft.content.clone(),
        thread_id: None,
    };
    record_import_event(ic_widget::ambient::log::LogEvent::Surfaced {
        id: suggestion.id.clone(),
        key: suggestion.key.clone(),
        source: suggestion.source.clone(),
        headline: suggestion.headline.clone(),
    });
    *state.pending_import.lock().await = Some(PendingImport {
        id: suggestion.id.clone(),
        folder: None,
        name: draft.name.clone(),
        skill_md: draft.content,
        overwrite,
    });
    let _ = app.emit("ambient://suggestion", &suggestion);
    update_character(&app, |inputs| inputs.suggestion_pending = true).await;
    result.drafted = Some(draft.name);
    Ok(result)
}

/// Answer a suggestion: Accept, or Not now.
///
/// Accept means "show me": the app switches both windows to the thread the run
/// landed in, and opens the dashboard on it. "Not now" is recorded with its
/// timestamp — it is never deleted, and it quiets that source for an hour.
#[tauri::command]
async fn respond_suggestion(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
    accepted: bool,
) -> Result<(), String> {
    // A pending skill import is answered first (Phase 7c): it exists even with
    // ambient off, and its consent must not depend on the ambient service.
    {
        let mut pending = state.pending_import.lock().await;
        if pending.as_ref().is_some_and(|import| import.id == id) {
            let import = pending.take().expect("just matched");
            drop(pending);
            update_character(&app, |inputs| inputs.suggestion_pending = false).await;

            let event = if accepted {
                ic_widget::ambient::log::LogEvent::Accepted {
                    id: import.id.clone(),
                    key: format!("skill-import:{}", import.name),
                    source: format!("import:{}", import.name),
                }
            } else {
                ic_widget::ambient::log::LogEvent::Dismissed {
                    id: import.id.clone(),
                    key: format!("skill-import:{}", import.name),
                    source: format!("import:{}", import.name),
                }
            };
            record_import_event(event);
            if !accepted {
                return Ok(());
            }

            // The approved text installs verbatim — the folder is only re-read
            // for its bundle files, never for the SKILL.md the user consented to.
            // A git re-sync (overwrite) removes the same-named installed skill
            // first, so the update is applied rather than refused as a duplicate.
            let result = skills_root().and_then(|root| {
                if import.overwrite {
                    let _ = ic_widget::skills::remove(&root, &import.name);
                }
                match &import.folder {
                    Some(folder) => {
                        ic_widget::skill_import::install(folder, &import.skill_md, &root)
                    }
                    // A studied repo left no folder behind — the reviewed text is
                    // the whole skill, written by the same validated file write
                    // the 7b draft path uses.
                    None => ic_widget::ambient::reflection::install(&root, &import.skill_md),
                }
            });
            let installed = match result {
                Ok(name) => {
                    tracing::info!(skill = %name, "imported a skill with the user's consent");
                    SkillInstallResult {
                        ok: true,
                        name: Some(name),
                        error: None,
                    }
                }
                Err(error) => {
                    tracing::error!(%error, "the approved skill import did not install");
                    SkillInstallResult {
                        ok: false,
                        name: None,
                        error: Some(error),
                    }
                }
            };
            let _ = app.emit("ambient://install-result", &installed);
            return Ok(());
        }
    }

    let Some(service) = state.ambient.lock().await.clone() else {
        return Ok(());
    };
    let answered = service.respond(&id, accepted).await;
    update_character(&app, |inputs| inputs.suggestion_pending = false).await;

    // An accepted skill draft is a consent (Phase 7b): install it now,
    // deterministically — the user approved this exact text, so no LLM sits
    // between the yes and the write. The runtime would not have prompted
    // (Phase 4), which is why this gate lives here and nowhere else.
    if let Some(suggestion) = &answered
        && accepted
        && suggestion.kind == SuggestionKind::SkillDraft
    {
        let result = skills_root()
            .and_then(|root| ic_widget::ambient::reflection::install(&root, &suggestion.body));
        let installed = match result {
            Ok(name) => {
                tracing::info!(skill = %name, "installed an approved skill draft");
                SkillInstallResult {
                    ok: true,
                    name: Some(name),
                    error: None,
                }
            }
            Err(error) => {
                tracing::error!(%error, "the approved skill draft did not install");
                SkillInstallResult {
                    ok: false,
                    name: None,
                    error: Some(error),
                }
            }
        };
        let _ = app.emit("ambient://install-result", &installed);
        return Ok(());
    }

    if let Some(suggestion) = answered
        && accepted
        && let Some(thread_id) = suggestion.thread_id
        && let Ok(thread_id) = ThreadId::new(&thread_id)
    {
        // Point both windows at the run's own thread — otherwise "show me" would
        // open a conversation that does not contain the thing being shown.
        follow_thread(&app, &state, thread_id).await?;
        if let Err(error) = show_dashboard(&app) {
            tracing::warn!(%error, "could not open the dashboard for an accepted suggestion");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- voice

/// Where the voice models (whisper, Piper voice + exe) live.
fn voice_root() -> Result<PathBuf, String> {
    dirs::data_local_dir()
        .map(|base| base.join("IronClaw Desktop").join("voice"))
        .ok_or_else(|| "could not locate the local application data directory".to_string())
}

/// Where the user's trained wakeword models (`.rpw`) live.
///
/// **The user's data directory, not the app's resource directory.** A wake word is
/// recorded *by this user, on this machine*, so it belongs beside their models and
/// settings — not inside the bundle. Writing it to the resource dir would fail
/// outright on an installed app (Program Files is read-only) and, in a dev build,
/// would be wiped by the next `cargo build`. The user would train a wake word, watch
/// it work, and find it gone tomorrow.
fn voice_wake_dir(app: &AppHandle) -> PathBuf {
    let _ = app;
    voice_root()
        .map(|root| root.join("wake"))
        .unwrap_or_else(|_| PathBuf::from("wake"))
}

/// Start voice in the background if the user has enabled it. Provisioning may
/// download the speech models on first run, so this never blocks the UI.
fn maybe_start_voice(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let settings = app
            .state::<AppState>()
            .settings_store
            .load()
            .unwrap_or_default();
        if settings.voice_enabled {
            start_voice(app).await;
        }
    });
}

/// Build the pipeline's widget-side seams and start it, storing the service in app
/// state. Best-effort: any failure leaves voice unavailable, never crashes the app.
///
/// Reads `voice_muted` from settings itself — and re-checks `voice_enabled` after
/// the (potentially minutes-long) model provisioning — because both can change
/// while the download runs: capturing them at spawn time shipped a mic that opened
/// unmuted after the user muted, and stayed hot after the user disabled voice.
async fn start_voice(app: AppHandle) {
    let job = Arc::clone(&app.state::<AppState>().job);
    let models_root = match voice_root() {
        Ok(root) => root,
        Err(error) => {
            tracing::warn!(%error, "no voice model directory; voice disabled");
            return;
        }
    };
    let downloader = match Downloader::new() {
        Ok(downloader) => downloader,
        Err(error) => {
            tracing::warn!(%error, "could not init the downloader; voice disabled");
            return;
        }
    };
    let wake_dir = voice_wake_dir(&app);

    // A gateway client on demand — voice may be ready before the gateway is.
    let provider: ic_widget::voice::ClientProvider = {
        let app = app.clone();
        Arc::new(move || {
            let app = app.clone();
            Box::pin(async move { app.state::<AppState>().client().await.ok() })
        })
    };

    // Voice state → the mic indicator (a Tauri event) and the character's voice_*
    // signals (separate from the typed-chat pair — see `CharacterInputs`). One
    // consumer task applies the transitions **in order**: spawning a task per
    // transition raced them, and rapid Listening→Transcribing→Sending bursts could
    // settle the character on a stale state.
    let (state_tx, mut state_rx) = tokio::sync::mpsc::unbounded_channel::<ic_voice::VoiceState>();
    {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(voice_state) = state_rx.recv().await {
                let _ = app.emit("voice://state", voice_state);
                update_character(&app, |inputs| {
                    inputs.voice_listening = matches!(voice_state, ic_voice::VoiceState::Listening);
                    inputs.voice_thinking = matches!(
                        voice_state,
                        ic_voice::VoiceState::Transcribing | ic_voice::VoiceState::Sending
                    );
                    inputs.voice_speaking = matches!(voice_state, ic_voice::VoiceState::Speaking);
                })
                .await;
            }
        });
    }
    let on_state: ic_voice::StateFn = Arc::new(move |voice_state: ic_voice::VoiceState| {
        // Unbounded so a transition is never dropped; the consumer keeps order.
        let _ = state_tx.send(voice_state);
    });

    // TTS amplitude → the character's mouth (lip sync), replacing the Phase 3 stub.
    let amplitude: ic_voice::AmplitudeSink = {
        let app = app.clone();
        Arc::new(move |level: f32| {
            let _ = app.emit("voice://amplitude", level);
        })
    };

    // Read the mute state and the selected voice NOW (post-provisioning they may be
    // stale, so mute is re-read below — but the pipeline needs values to start with).
    let (start_muted, voice_id) = app
        .state::<AppState>()
        .settings_store
        .load()
        .map(|s| (s.voice_muted, s.voice_id))
        .unwrap_or((false, None));

    // Voice speaks on the app's conversation, not one of its own — so a spoken
    // question lands in the same transcript the dashboard shows and the same bubble
    // the character speaks from.
    let threads: ic_widget::voice::ThreadProvider = {
        let app = app.clone();
        Arc::new(move || {
            let app = app.clone();
            Box::pin(async move {
                let state = app.state::<AppState>();
                let mut current = state.thread.lock().await;
                if let Some(thread_id) = current.as_ref() {
                    return Some(thread_id.clone());
                }
                let thread_id = open_thread(&app, &state).await.ok()?;
                *current = Some(thread_id.clone());
                Some(thread_id)
            })
        })
    };

    // Re-read every turn: the user may switch between read and hear mid-conversation.
    let speaks: ic_widget::voice::SpeaksFn = {
        let app = app.clone();
        Arc::new(move || {
            app.state::<AppState>()
                .settings_store
                .load()
                .map(|settings| settings.reply_mode.speaks())
                .unwrap_or(true)
        })
    };

    let input_device: ic_widget::voice::InputDeviceFn = {
        let app = app.clone();
        Arc::new(move || chosen_microphone(&app.state::<AppState>()))
    };

    // What the microphone actually heard. Without this the voice pipeline is a
    // black box with five stages and one symptom: a muted mic, a device that hears
    // nothing, a wake word that never fired, a transcript whisper could not make
    // out, and a failed gateway turn are indistinguishable from outside — all of
    // them are just "nothing happened".
    let on_transcript: ic_voice::TranscriptFn = {
        let app = app.clone();
        Arc::new(move |text: String| {
            tracing::info!(heard = %text, "voice transcript");
            let _ = app.emit("voice://transcript", text);
        })
    };

    let service = ic_widget::voice::start(
        job,
        models_root,
        wake_dir,
        downloader,
        provider,
        threads,
        speaks,
        input_device,
        on_transcript,
        on_state,
        amplitude,
        start_muted,
        voice_id,
    )
    .await;

    let Some(service) = service else { return };

    // Provisioning may have taken minutes; the user may have changed their mind.
    // Re-read the settings and honour them before going live.
    let settings = app
        .state::<AppState>()
        .settings_store
        .load()
        .unwrap_or_default();
    if !settings.voice_enabled {
        tracing::info!("voice was disabled while provisioning; not starting");
        service.shutdown().await;
        return;
    }
    if settings.voice_muted != start_muted {
        service.set_muted(settings.voice_muted).await;
    }

    let state = app.state::<AppState>();
    let duplicate = {
        let mut slot = state.voice.lock().await;
        if slot.is_some() {
            // A concurrent start won the race (launch + a quick enable toggle both
            // spawn one). Keep the installed pipeline; wind this one down.
            Some(service)
        } else {
            *slot = Some(service);
            None
        }
    };
    match duplicate {
        Some(service) => {
            tracing::info!("a voice pipeline is already running; discarding the duplicate");
            service.shutdown().await;
        }
        None => tracing::info!("voice is ready"),
    }
}

/// Toggle the microphone from the tray, persisting the new state.
fn toggle_voice_mute(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let muted = {
            let voice = state.voice.lock().await;
            match voice.as_ref() {
                Some(service) => service.toggle_mute().await,
                None => {
                    tracing::info!("microphone toggle ignored: voice is not running");
                    return;
                }
            }
        };
        if let Err(error) = state.update_settings(|settings| settings.voice_muted = muted) {
            tracing::warn!(%error, "could not persist the mute state");
        }
        tracing::info!(muted, "microphone toggled from the tray");
    });
}

/// Start listening now, if voice is running and unmuted — the summon-hotkey path.
fn trigger_voice_listen(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Some(service) = app.state::<AppState>().voice.lock().await.as_ref() {
            service.trigger_listen().await;
        }
    });
}

/// Whether the first-run setup wizard should be shown (setup not yet completed).
#[tauri::command]
async fn needs_setup(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let settings = state.settings_store.load().map_err(|e| e.to_string())?;
    Ok(!settings.setup_complete)
}

/// Mark first-run setup done, so the wizard does not show again.
#[tauri::command]
async fn complete_setup(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.update_settings(|settings| settings.setup_complete = true)?;
    Ok(())
}

/// One recorded utterance of the wake phrase.
#[derive(Serialize)]
struct WakeSample {
    /// How loud it was, so the wizard can tell the user their mic is muted rather
    /// than letting them record three silent takes and fail at the end.
    peak: f32,
    /// How many the user has banked so far, and how many are needed.
    recorded: usize,
    needed: usize,
}

/// Record `seconds` of audio from the chosen microphone.
///
/// Runs on a blocking thread: `CpalCapture::start` probes each candidate device for
/// up to half a second, which must not sit on an async worker.
async fn record_samples(device: Option<String>, seconds: f32) -> Result<Vec<f32>, String> {
    tokio::task::spawn_blocking(move || {
        use ic_voice::Capture as _; // `ring()` lives on the trait.
        // A fresh capture per take, released as soon as the take ends: holding the
        // mic open across the whole wizard would light the mic indicator for minutes
        // while the user reads instructions.
        let capture = ic_voice::CpalCapture::start_on(device.as_deref(), seconds + 0.5)
            .map_err(|error| format!("could not open the microphone: {error}"))?;
        let ring = capture.ring();
        std::thread::sleep(std::time::Duration::from_secs_f32(seconds));
        drop(capture);
        Ok(ring.latest((ic_voice::format::SAMPLE_RATE as f32 * seconds) as usize))
    })
    .await
    .map_err(|error| format!("the recording task failed: {error}"))?
}

/// The microphone the user chose, or `None` to follow the OS default.
fn chosen_microphone(state: &AppState) -> Option<String> {
    state
        .settings_store
        .load()
        .ok()
        .and_then(|settings| settings.input_device)
}

/// Which microphone is chosen (`None` follows the OS default).
#[derive(Serialize)]
struct VoiceSettings {
    input_device: Option<String>,
}

#[tauri::command]
async fn voice_settings(state: tauri::State<'_, AppState>) -> Result<VoiceSettings, String> {
    Ok(VoiceSettings {
        input_device: chosen_microphone(&state),
    })
}

/// Every input device on the machine, OS default first.
#[tauri::command]
async fn input_devices() -> Result<Vec<String>, String> {
    tokio::task::spawn_blocking(ic_voice::input_devices)
        .await
        .map_err(|error| format!("could not list the microphones: {error}"))
}

/// Choose the microphone. Takes effect on the next recording / unmute.
#[tauri::command]
async fn set_input_device(
    state: tauri::State<'_, AppState>,
    device: Option<String>,
) -> Result<(), String> {
    state.update_settings(|settings| settings.input_device = device.clone())?;
    tracing::info!(device = ?device, "the microphone was changed");
    Ok(())
}

/// Listen for a moment and report how loud it was.
///
/// This exists because the failure it catches is invisible: a Bluetooth headset's
/// HFP endpoint takes the default input slot, opens cleanly, and delivers a steady
/// stream of near-silence. Nothing errors; the user simply is not heard. A number
/// they can watch move while they talk is the only honest way to tell them which
/// microphone actually works.
#[tauri::command]
async fn test_microphone(state: tauri::State<'_, AppState>) -> Result<f32, String> {
    let device = chosen_microphone(&state);
    let samples = record_samples(device, MIC_TEST_SECONDS).await?;
    Ok(ic_voice::sample_peak(&samples))
}

/// Record one utterance of the wake phrase.
///
/// The wake word is the assistant's *name*, and rustpotter is a reference-model
/// spotter — a wake word simply *is* a few recordings of someone saying it. So the
/// model is trained here, on this machine, from this user's voice. Nothing is
/// uploaded, and this is the only way the app can have a wake word at all: there is
/// no pretrained one to ship (openWakeWord's are non-commercial).
#[tauri::command]
async fn record_wake_sample(state: tauri::State<'_, AppState>) -> Result<WakeSample, String> {
    let device = chosen_microphone(&state);
    let samples = record_samples(device, WAKE_TAKE_SECONDS).await?;
    let peak = ic_voice::sample_peak(&samples);

    let mut takes = state.wake_takes.lock().await;
    takes.push(samples);
    Ok(WakeSample {
        peak,
        recorded: takes.len(),
        needed: ic_voice::WAKE_MIN_SAMPLES,
    })
}

/// How long the microphone test listens.
const MIC_TEST_SECONDS: f32 = 2.0;

/// Start listening now — the same thing the summon hotkey does.
///
/// Exists because the two ways in are both easy to miss: the hotkey is invisible
/// (and its default was already taken on this machine), and the wake word does not
/// exist until the user records one. A button that says "Talk" is the honest third
/// door.
#[tauri::command]
async fn start_listening(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let voice = state.voice.lock().await;
    let Some(service) = voice.as_ref() else {
        return Err("Voice is not running — enable it first.".to_string());
    };
    if service.is_muted() {
        return Err("The microphone is muted.".to_string());
    }
    service.trigger_listen().await;
    Ok(())
}

/// Speak a reply aloud, if the user asked for replies to be heard.
///
/// The *decision* lives here, not in the UI: `reply_mode` is persisted state, and a
/// webview that had to consult it before every call would eventually get it wrong.
/// Silently does nothing when the mode is `read`, or when voice is not running.
#[tauri::command]
async fn speak_reply(state: tauri::State<'_, AppState>, text: String) -> Result<(), String> {
    let settings = state.settings_store.load().map_err(|e| e.to_string())?;
    if !settings.reply_mode.speaks() {
        tracing::debug!(mode = ?settings.reply_mode, "not speaking the reply: reply mode is read-only");
        return Ok(());
    }
    let voice = state.voice.lock().await;
    let Some(service) = voice.as_ref() else {
        tracing::warn!("asked to speak a reply, but voice is not running");
        return Ok(());
    };
    tracing::info!(chars = text.len(), "speaking a typed reply");
    service.speak(text).await;
    Ok(())
}

/// Whether a wake word has actually been trained.
///
/// Recording takes is not training: the user can record three times, never press
/// "Teach it", and be left with a wake word that does not exist — which is exactly
/// what happened. The UI needs to be able to say so.
#[tauri::command]
async fn has_wake_word(app: AppHandle) -> Result<bool, String> {
    let wake_dir = voice_wake_dir(&app);
    Ok(!ic_voice::bundled_wake_models(&wake_dir).is_empty())
}

/// Throw away the recordings and start over.
#[tauri::command]
async fn reset_wake_samples(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.wake_takes.lock().await.clear();
    Ok(())
}

/// Train the wake word from the recordings, save it, and start listening for it.
///
/// The model lands in the directory the voice pipeline scans at start — so
/// training it under a *running* pipeline changes nothing until that pipeline is
/// replaced. This restarts voice itself rather than telling the user to toggle
/// it: "record three takes, and now it answers to its name" is the whole feature,
/// and a wake word that only works after the next launch is one that looks broken.
#[tauri::command]
async fn train_wake_word(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    name: String,
) -> Result<String, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("give the assistant a name first — the name is the wake word".to_string());
    }

    let takes = state.wake_takes.lock().await.clone();
    let wake_dir = voice_wake_dir(&app);
    let path = ic_voice::train_wake_word(&wake_dir, &name, &takes).map_err(|e| e.to_string())?;

    // The takes are spent. Keeping them would stack this training's recordings
    // onto the next one's, so a retrain would silently be trained on both voices.
    state.wake_takes.lock().await.clear();

    // Swap the running pipeline (which is holding a `NullWakeWord`, since there
    // was no model when it started) for one that spots the phrase.
    if state.settings_store.load().is_ok_and(|s| s.voice_enabled) {
        restart_voice(&app).await;
        tracing::info!(%name, "wake word trained; the pipeline is now listening for it");
    } else {
        tracing::info!(%name, "wake word trained; it takes effect when voice is switched on");
    }
    Ok(path.display().to_string())
}

/// Replace the running voice pipeline with a fresh one.
///
/// The pipeline resolves its wake-word models, input device, and TTS voice when
/// it starts, so anything that changes those needs the pipeline rebuilt rather
/// than reconfigured — the same shape as the gateway's boot-time environment.
async fn restart_voice(app: &AppHandle) {
    if let Some(service) = app.state::<AppState>().voice.lock().await.take() {
        service.shutdown().await;
    }
    start_voice(app.clone()).await;
}

/// How long one wake-phrase take records for. A name is a word or two; a longer
/// window mostly banks silence, which drags the model's reference features toward
/// the room rather than the voice.
const WAKE_TAKE_SECONDS: f32 = 1.8;

/// The local profile: who the user is, what the assistant is called, and how it
/// answers. There is no account here — this is a single-user desktop app, and
/// these are the facts the agent is told about itself and the person it is
/// talking to.
#[derive(Clone, Serialize)]
struct Profile {
    user_name: String,
    assistant_name: String,
    reply_mode: ReplyMode,
}

#[tauri::command]
async fn profile(state: tauri::State<'_, AppState>) -> Result<Profile, String> {
    let settings = state.settings_store.load().map_err(|e| e.to_string())?;
    Ok(Profile {
        user_name: settings.user_name,
        assistant_name: settings.assistant_name,
        reply_mode: settings.reply_mode,
    })
}

/// Save the profile and re-teach the agent its persona.
///
/// The persona lands in the gateway's system-prompt file, which the runtime
/// re-reads on every run — so a rename takes effect on the *next turn*, with no
/// gateway restart and no lost thread.
#[tauri::command]
async fn set_profile(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    user_name: String,
    assistant_name: String,
    reply_mode: ReplyMode,
) -> Result<(), String> {
    let settings = state.update_settings(|settings| {
        settings.user_name = user_name.trim().to_string();
        settings.assistant_name = assistant_name.trim().to_string();
        settings.reply_mode = reply_mode;
    })?;

    if let Ok(home) = reborn_home() {
        ic_widget::persona::apply(&home, &settings);
    }
    // The widget renders the name and obeys the reply mode, so tell it now rather
    // than making it poll.
    let _ = app.emit(
        "profile://changed",
        Profile {
            user_name: settings.user_name.clone(),
            assistant_name: settings.assistant_name.clone(),
            reply_mode: settings.reply_mode,
        },
    );
    Ok(())
}

/// The voice UI status: whether it is enabled, actually running, and muted.
#[derive(Serialize)]
struct VoiceStatus {
    enabled: bool,
    running: bool,
    muted: bool,
}

#[tauri::command]
async fn voice_status(state: tauri::State<'_, AppState>) -> Result<VoiceStatus, String> {
    let settings = state.settings_store.load().map_err(|e| e.to_string())?;
    let voice = state.voice.lock().await;
    let (running, muted) = match voice.as_ref() {
        Some(service) => (true, service.is_muted()),
        None => (false, settings.voice_muted),
    };
    Ok(VoiceStatus {
        enabled: settings.voice_enabled,
        running,
        muted,
    })
}

/// Mute or unmute the microphone, persisting the choice.
#[tauri::command]
async fn set_voice_muted(state: tauri::State<'_, AppState>, muted: bool) -> Result<(), String> {
    state.update_settings(|settings| settings.voice_muted = muted)?;
    if let Some(service) = state.voice.lock().await.as_ref() {
        service.set_muted(muted).await;
    }
    Ok(())
}

/// One voice in the picker (Phase 8c): catalog metadata plus whether it is already
/// downloaded and whether it is the current selection.
#[derive(Serialize)]
struct VoiceOption {
    id: String,
    display_name: String,
    accent: String,
    /// Whether this voice's model is already on disk (no download needed).
    installed: bool,
    /// Whether this is the currently-selected voice.
    selected: bool,
    /// The model download size (the config is a few KB and is not counted).
    size_bytes: u64,
}

/// The curated TTS voices, each marked installed/selected for the Voice panel.
#[tauri::command]
async fn voice_catalog(state: tauri::State<'_, AppState>) -> Result<Vec<VoiceOption>, String> {
    let settings = state.settings_store.load().map_err(|e| e.to_string())?;
    let selected = ic_voice::voice_or_default(settings.voice_id.as_deref()).id;
    let root = voice_root().map_err(|e| e.to_string())?;
    Ok(ic_voice::VOICES
        .iter()
        .map(|voice| VoiceOption {
            id: voice.id.to_string(),
            display_name: voice.display_name.to_string(),
            accent: voice.accent.to_string(),
            installed: ic_voice::VoiceAssets::voice_installed(&root, voice),
            selected: voice.id == selected,
            size_bytes: voice.onnx.size_bytes,
        })
        .collect())
}

/// Progress of a voice download, streamed to the panel on `voice://voice-download`.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum VoiceDownloadEvent {
    Progress {
        id: String,
        downloaded: u64,
        total: Option<u64>,
        fraction: Option<f64>,
    },
    Finished {
        id: String,
        ok: bool,
        error: Option<String>,
    },
}

/// Select the TTS voice (Phase 8c).
///
/// Persists the choice always. If voice is **running**, it also downloads the
/// voice (idempotent — the shared whisper/piper.exe are already present, so only
/// this voice's ~63 MB model transfers) with progress on `voice://voice-download`,
/// then restarts the pipeline onto it. The restart stops any in-flight playback
/// before rebuilding (`VoiceService::shutdown` → the driver's exit path calls
/// `playback.stop()`), so switching mid-sentence releases the audio device cleanly
/// rather than deadlocking it. If voice is **off**, the choice is saved and applies
/// when voice is next enabled — no download is forced on a user who only browsed.
#[tauri::command]
async fn set_voice(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let voice = ic_voice::find_voice(&id).ok_or_else(|| format!("unknown voice: {id}"))?;
    state.update_settings(|settings| settings.voice_id = Some(id.clone()))?;

    // Voice off ⇒ persist only; it downloads and applies on the next enable.
    if state.voice.lock().await.is_none() {
        tracing::info!(voice = %id, "voice selection saved; applies when voice is enabled");
        return Ok(());
    }

    let root = voice_root().map_err(|e| e.to_string())?;
    let downloader = Downloader::new().map_err(|e| e.to_string())?;
    let progress: ic_voice::AssetProgress = {
        let app = app.clone();
        let id = id.clone();
        Arc::new(move |_label: &str, snapshot: Progress| {
            let _ = app.emit(
                "voice://voice-download",
                VoiceDownloadEvent::Progress {
                    id: id.clone(),
                    downloaded: snapshot.downloaded,
                    total: snapshot.total,
                    fraction: snapshot.fraction(),
                },
            );
        })
    };

    let outcome = ic_voice::VoiceAssets::ensure(&root, &downloader, voice, Some(progress)).await;
    if let Err(error) = outcome {
        let _ = app.emit(
            "voice://voice-download",
            VoiceDownloadEvent::Finished {
                id: id.clone(),
                ok: false,
                error: Some(error.to_string()),
            },
        );
        return Err(format!("could not download the voice: {error}"));
    }

    // Files are present; rebuild the pipeline onto the new voice.
    restart_voice(&app).await;
    let _ = app.emit(
        "voice://voice-download",
        VoiceDownloadEvent::Finished {
            id,
            ok: true,
            error: None,
        },
    );
    Ok(())
}

/// Turn voice on or off, persisting the choice. Enabling provisions and starts it
/// in the background (a first-run download does not block this call). Disabling
/// winds the pipeline down in the background too: `VoiceService::shutdown` waits
/// for the driver task, and holding this command (and the `state.voice` lock) on
/// that would freeze the settings toggle.
#[tauri::command]
async fn set_voice_enabled(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    state.update_settings(|settings| settings.voice_enabled = enabled)?;

    match enabled {
        // Provision + start in the background (a first-run download must not block
        // this call). Already running → nothing to do; start_voice itself re-checks
        // the settings after provisioning and discards duplicates.
        true => {
            if state.voice.lock().await.is_none() {
                let app = app.clone();
                tauri::async_runtime::spawn(async move { start_voice(app).await });
            }
        }
        false => {
            if let Some(service) = state.voice.lock().await.take() {
                tauri::async_runtime::spawn(async move { service.shutdown().await });
            }
        }
    }
    Ok(())
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItemBuilder::with_id("show", "Show / hide widget").build(app)?;
    let dashboard = MenuItemBuilder::with_id("dashboard", "Open dashboard").build(app)?;
    let mic = MenuItemBuilder::with_id("voice_mute", "Toggle microphone").build(app)?;
    let ambient = MenuItemBuilder::with_id("ambient", "Ambient suggestions on / off").build(app)?;
    let reset = MenuItemBuilder::with_id("reset", "Reset widget position").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
    let menu = MenuBuilder::new(app)
        .items(&[&show, &dashboard, &mic, &ambient, &reset, &quit])
        .build()?;

    let Some(icon) = app.default_window_icon().cloned() else {
        // Without an icon there is nothing to click. The hotkey still works.
        tracing::warn!("no application icon; skipping the tray");
        return Ok(());
    };

    TrayIconBuilder::with_id("tray")
        .icon(icon)
        .tooltip("IronClaw")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => toggle_widget(app),
            "dashboard" => {
                if let Err(error) = show_dashboard(app) {
                    tracing::warn!(%error, "could not open the dashboard");
                }
            }
            "voice_mute" => toggle_voice_mute(app),
            "ambient" => toggle_ambient(app),
            "reset" => reset_widget_position(app),
            // Dropping the app drops the `ProcessJob`, which kills the gateway
            // and anything it spawned.
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

/// Flip ambient mode from the tray. It restarts the gateway (the trigger poller is
/// a boot-time switch), so it runs in the background rather than freezing the menu.
fn toggle_ambient(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let enabled = match state.settings_store.load() {
            Ok(settings) => !settings.ambient_enabled,
            Err(error) => {
                tracing::warn!(%error, "could not read the ambient setting");
                return;
            }
        };
        let settings = match state.update_settings(|settings| settings.ambient_enabled = enabled) {
            Ok(settings) => settings,
            Err(error) => {
                tracing::warn!(%error, "could not save the ambient setting");
                return;
            }
        };
        tracing::info!(enabled, "ambient mode toggled from the tray");
        stop_ambient(&app).await;
        restart_gateway(app.clone(), settings.active_provider).await;
    });
}

/// A widget stranded offscreen cannot be dragged back. This is the escape hatch.
fn reset_widget_position(app: &AppHandle) {
    let state = app.state::<AppState>();
    if let Ok(mut guard) = state.window_state.lock() {
        guard.forget_all();
        if let Err(error) = state.window_store.save(&guard) {
            tracing::warn!(%error, "could not clear the saved widget positions");
        }
    }
    if let Some(window) = app.get_webview_window(WIDGET) {
        let _ = window.center();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Bindings to try, in order, when the user has not pinned one.
///
/// Ctrl+Alt+Space is commonly taken (it is a default in several IME and launcher
/// tools), and a hotkey that fails to register is *silent*: the tray still works,
/// so the app looks fine while push-to-talk never fires — which makes voice look
/// broken rather than unbound. So we fall down a ladder until something sticks.
const HOTKEY_LADDER: [&str; 4] = [
    "Ctrl+Alt+Space",
    "Ctrl+Shift+Space",
    "Ctrl+Alt+A",
    "Ctrl+Shift+A",
];

/// Register the summon hotkey, honoring the user's binding and falling back.
///
/// Returns the binding that actually took, or `None` if every candidate was
/// occupied. The winner is persisted so the settings UI can show the truth rather
/// than the intention.
fn register_summon_hotkey(app: &AppHandle) -> Option<String> {
    let configured = app
        .state::<AppState>()
        .settings_store
        .load()
        .ok()
        .and_then(|settings| settings.summon_hotkey);

    // The user's choice first (and *only*, if they made one — silently drifting off
    // a hotkey someone deliberately picked would be worse than not binding it).
    let candidates: Vec<String> = match configured {
        Some(binding) => vec![binding],
        None => HOTKEY_LADDER.iter().map(|s| s.to_string()).collect(),
    };

    for binding in candidates {
        let Ok(shortcut) = binding.parse::<Shortcut>() else {
            tracing::warn!(%binding, "not a valid hotkey");
            continue;
        };
        let result = app
            .global_shortcut()
            .on_shortcut(shortcut, |app, _shortcut, event| {
                // Fire on press only; a release would toggle straight back.
                if event.state() == ShortcutState::Pressed {
                    toggle_widget(app);
                    // Summon doubles as push-to-talk: if voice is running, start
                    // listening. A no-op when voice is off.
                    trigger_voice_listen(app);
                }
            });
        match result {
            Ok(()) => {
                tracing::info!(%binding, "the summon hotkey is live");
                let _ = app
                    .state::<AppState>()
                    .update_settings(|settings| settings.summon_hotkey = Some(binding.clone()));
                return Some(binding);
            }
            Err(error) => {
                tracing::warn!(%binding, %error, "that hotkey is taken; trying the next one");
            }
        }
    }

    tracing::warn!(
        "no summon hotkey could be registered; use the tray. Push-to-talk will not fire."
    );
    None
}

/// The hotkey currently bound, or `None` if none took.
#[tauri::command]
async fn summon_hotkey(state: tauri::State<'_, AppState>) -> Result<Option<String>, String> {
    let settings = state.settings_store.load().map_err(|e| e.to_string())?;
    Ok(settings.summon_hotkey)
}

/// Rebind the summon hotkey. Fails (leaving the old one bound) if the new binding
/// is unparseable or already owned by another application — so the user gets told,
/// rather than silently losing their hotkey.
#[tauri::command]
async fn set_summon_hotkey(app: AppHandle, binding: String) -> Result<(), String> {
    let shortcut: Shortcut = binding
        .parse()
        .map_err(|_| format!("{binding:?} is not a valid hotkey"))?;

    // Drop whatever we hold before claiming the new one; re-registering the same
    // combination otherwise fails against ourselves.
    let _ = app.global_shortcut().unregister_all();

    app.global_shortcut()
        .on_shortcut(shortcut, |app, _shortcut, event| {
            if event.state() == ShortcutState::Pressed {
                toggle_widget(app);
                trigger_voice_listen(app);
            }
        })
        .map_err(|error| {
            // We just gave up the old binding, so put *something* back rather than
            // leaving the user with no hotkey at all.
            let restored = register_summon_hotkey(&app);
            match restored {
                Some(binding) => {
                    format!("{binding} is still bound — the new hotkey is taken ({error})")
                }
                None => {
                    format!("that hotkey is taken, and the old one could not be restored: {error}")
                }
            }
        })?;

    app.state::<AppState>()
        .update_settings(|settings| settings.summon_hotkey = Some(binding.clone()))?;
    tracing::info!(%binding, "the summon hotkey was rebound");
    Ok(())
}

/// Send `tracing` output to stderr.
///
/// Without a subscriber every `tracing::warn!` in this app is silently dropped,
/// which is how a widget that fails to reach its gateway ends up with nothing to
/// show for it. `RUST_LOG` overrides the default.
fn init_logging() {
    use tracing_subscriber::EnvFilter;

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("ic_widget=info,warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

/// `%LOCALAPPDATA%\IronClaw Desktop` — everything this app writes lives under here.
fn data_root() -> Option<PathBuf> {
    dirs::data_local_dir().map(|base| base.join("IronClaw Desktop"))
}

/// Remove everything this app leaves on the machine: the Credential Manager entries
/// and the per-user data directory (settings, models, libSQL store, browser
/// profile). Invoked by the installer's uninstall custom action
/// (`ic-widget.exe --uninstall-cleanup`), since an MSI removes neither on its own.
///
/// Best-effort and idempotent: a missing entry or directory is success, and a
/// failure to clear one thing is logged but does not stop the rest — a half-finished
/// cleanup is worse than a reported one.
fn run_uninstall_cleanup() {
    tracing::info!("uninstall cleanup: removing credentials and app data");
    if let Err(error) = SecretStore::new().clear_all() {
        tracing::warn!(%error, "uninstall cleanup: could not clear all credentials");
    }
    match data_root() {
        Some(dir) if dir.exists() => match std::fs::remove_dir_all(&dir) {
            Ok(()) => tracing::info!(dir = %dir.display(), "uninstall cleanup: removed app data"),
            Err(error) => {
                tracing::warn!(dir = %dir.display(), %error, "uninstall cleanup: could not remove app data")
            }
        },
        Some(dir) => {
            tracing::info!(dir = %dir.display(), "uninstall cleanup: no app data to remove")
        }
        None => tracing::warn!("uninstall cleanup: could not locate the app data directory"),
    }
}

fn main() {
    init_logging();

    // The installer's uninstall step runs us with this flag to clean up what the
    // MSI cannot: our Credential Manager entries and per-user data directory. Do it
    // and exit before touching Tauri — there is no UI to bring up.
    if std::env::args().any(|arg| arg == "--uninstall-cleanup") {
        run_uninstall_cleanup();
        return;
    }

    tauri::Builder::default()
        // A second launch focuses the widget rather than starting a second
        // gateway on a second port against the same database.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            toggle_widget(app);
        }))
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            gateway_state,
            gateway_log,
            current_thread,
            new_thread,
            send_message,
            cancel_run,
            fetch_timeline,
            resolve_gate,
            answer_browser_fill,
            canvas_content,
            list_threads,
            list_automations,
            local_model_status,
            provider_settings,
            set_provider_key,
            clear_provider_key,
            apply_provider,
            recommended_models,
            installed_models,
            download_model,
            cancel_download,
            remove_model,
            character_state,
            set_character_signals,
            character_settings,
            set_character,
            set_hit_mask,
            log_ui_error,
            open_dashboard,
            voice_status,
            set_voice_muted,
            set_voice_enabled,
            ambient_status,
            set_ambient_enabled,
            set_ambient_guardrails,
            set_reflection_enabled,
            use_model,
            set_cloud_fallback,
            preview_skill_import,
            request_skill_import,
            preview_repo_skills,
            request_repo_skill,
            study_repo,
            list_installed_skills,
            remove_installed_skill,
            watchers_status,
            set_watcher_kinds,
            set_watch_rules,
            set_thread_hidden,
            use_thread,
            test_provider,
            list_connectors,
            install_connector,
            set_connector_token,
            set_connector_enabled,
            google_oauth_status,
            set_google_oauth,
            clear_google_oauth,
            set_oauth_callback_port,
            authorize_google_connector,
            recover_auth_gate,
            respond_suggestion,
            needs_setup,
            profile,
            set_profile,
            summon_hotkey,
            set_summon_hotkey,
            record_wake_sample,
            reset_wake_samples,
            train_wake_word,
            input_devices,
            set_input_device,
            test_microphone,
            voice_settings,
            voice_catalog,
            set_voice,
            has_wake_word,
            speak_reply,
            start_listening,
            complete_setup,
        ])
        .setup(|app| {
            let store = WindowStateStore::at(WindowStateStore::default_path()?);
            let window_state = store.load()?;
            let job = Arc::new(ProcessJob::new()?);

            app.manage(AppState {
                gateway: Mutex::new(None),
                job: Arc::clone(&job),
                pump: Mutex::new(None),
                wake_takes: Mutex::new(Vec::new()),
                thread: Mutex::new(None),
                local_llm: Mutex::new(None),
                browser: Mutex::new(None),
                canvas: Mutex::new(None),
                last_canvas: std::sync::Mutex::new(None),
                download: Mutex::new(None),
                window_state: std::sync::Mutex::new(window_state),
                window_store: store,
                settings_store: SettingsStore::at(SettingsStore::default_path()?),
                character: Mutex::new(CharacterTracker::default()),
                hit_mask: std::sync::Mutex::new(None),
                voice: Mutex::new(None),
                settings_write: std::sync::Mutex::new(()),
                ambient: Mutex::new(None),
                ambient_task: Mutex::new(None),
                ambient_thread: Mutex::new(None),
                reflection_runs: Mutex::new(RunWatch::new()),
                pending_import: Mutex::new(None),
                repo_clone: Mutex::new(None),
                watcher_task: Mutex::new(None),
            });

            let handle = app.handle().clone();
            let widget = build_widget(&handle)?;
            restore_position(&widget, &app.state::<AppState>());
            widget.show()?;

            build_tray(&handle)?;
            register_summon_hotkey(&handle);
            #[cfg(windows)]
            spawn_interaction_watch(handle.clone());
            spawn_gateway(handle.clone());
            maybe_start_voice(handle.clone());

            // Persist the widget's position whenever the user drags it.
            widget.on_window_event(move |event| {
                if let tauri::WindowEvent::Moved(position) = event
                    && let Some(window) = handle.get_webview_window(WIDGET)
                {
                    remember_position(
                        &window,
                        &handle.state::<AppState>(),
                        WindowPosition {
                            x: position.x,
                            y: position.y,
                        },
                    );
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("the Tauri application should start");
}
