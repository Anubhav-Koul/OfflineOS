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

use std::path::PathBuf;
use std::sync::Arc;

use ic_llama::download::{Downloader, Progress, ProgressFn};
use ic_llama::{
    Digest, HubModel, LocalLlm, LocalLlmOptions, ModelId, ModelStore, SidecarState, SpawnHook,
    Verdict,
};
use ic_widget::canvas::{CallbackSink, CanvasServer};
use ic_widget::character::{self, CharacterInputs, CharacterState};
use ic_widget::error::Error;
use ic_widget::gateway_client::{
    ClientActionId, GateRef, GateResolution, GatewayClient, GatewayEvent, ProjectionItem, RunId,
    ThreadId,
};
use ic_widget::hit_test::HitMask;
use ic_widget::settings::{CharacterId, ProviderSelection, SettingsStore};
use ic_widget::supervisor::{GatewayConfig, GatewayState, GatewaySupervisor};
use ic_widget::window_state::{LayoutHash, MonitorInfo, WindowPosition};
use ic_widget::{BrowserSidecar, ProcessJob, RunPhase, SecretStore, WindowState, WindowStateStore};
use serde::Serialize;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tokio::sync::Mutex;

const WIDGET: &str = "widget";
const DASHBOARD: &str = "dashboard";
const CANVAS: &str = "canvas";

/// Ctrl+Alt+Space, chosen to dodge the common Windows and IDE bindings.
fn summon_shortcut() -> Shortcut {
    Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space)
}

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

/// Create a thread and start pumping its events to the UI.
#[tauri::command]
async fn create_thread(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
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
    update_character(&app, |inputs| inputs.run = None).await;

    tracing::info!(%thread_id, "the widget created a thread and started its event pump");
    Ok(thread_id.to_string())
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

#[derive(Serialize)]
struct CancelResult {
    already_terminal: bool,
}

#[tauri::command]
async fn cancel_run(
    state: tauri::State<'_, AppState>,
    thread_id: String,
    run_id: String,
) -> Result<CancelResult, String> {
    let client = state.client().await?;
    let thread_id = ThreadId::new(thread_id).map_err(user_facing)?;
    let run_id = RunId::new(run_id).map_err(user_facing)?;
    let outcome = client
        .cancel_run(&thread_id, &run_id)
        .await
        .map_err(user_facing)?;
    Ok(CancelResult {
        already_terminal: outcome.already_terminal,
    })
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

/// A row in the sessions panel.
#[derive(Serialize)]
struct UiThread {
    thread_id: String,
    /// `None` until the agent has titled the thread; the UI shows a placeholder.
    title: Option<String>,
}

/// The caller's threads, newest first. Threads survive gateway restarts (they
/// are persisted through the libSQL-backed root filesystem), so this is a stable
/// list, not a per-session one.
#[tauri::command]
async fn list_threads(state: tauri::State<'_, AppState>) -> Result<Vec<UiThread>, String> {
    let client = state.client().await?;
    let threads = client.list_threads(None).await.map_err(user_facing)?;
    Ok(threads
        .into_iter()
        .map(|thread| UiThread {
            thread_id: thread.thread_id.to_string(),
            title: thread.title,
        })
        .collect())
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
}

/// The provider panel's data: what is active, and the configurable providers.
#[derive(Serialize)]
struct UiProviderSettings {
    /// The selection the gateway is running on.
    active: ProviderSelection,
    /// The cloud providers that take an API key. The local model is a separate,
    /// always-available choice the UI offers alongside these.
    providers: Vec<UiProvider>,
}

/// The active selection and the configurable cloud providers, each flagged with
/// whether a key is stored. The key values never leave the credential store.
#[tauri::command]
async fn provider_settings(
    state: tauri::State<'_, AppState>,
) -> Result<UiProviderSettings, String> {
    let active = state
        .settings_store
        .load()
        .map_err(user_facing)?
        .active_provider;

    let secrets = SecretStore::new();
    let catalog = ic_widget::providers::api_key_providers().map_err(user_facing)?;
    let mut providers = Vec::with_capacity(catalog.len());
    for provider in catalog {
        // A keyring read failure is surfaced, not folded into "no key" — that
        // would let a transient store error read as an unconfigured provider.
        let has_key = secrets.has_provider_key(&provider).map_err(user_facing)?;
        providers.push(UiProvider {
            id: provider.id.clone(),
            description: provider.description.clone(),
            default_model: provider.default_model.clone(),
            has_key,
        });
    }
    Ok(UiProviderSettings { active, providers })
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
    // Load-modify-save: settings now carry more than the provider, and a
    // rebuilt literal here would silently reset the rest.
    let mut settings = state
        .settings_store
        .load()
        .map_err(|error| error.to_string())?;
    settings.active_provider = selection.clone();
    state
        .settings_store
        .save(&settings)
        .map_err(|error| error.to_string())?;

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
    Ok(())
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
    let mut settings = state
        .settings_store
        .load()
        .map_err(|error| error.to_string())?;
    settings.character = Some(id);
    state
        .settings_store
        .save(&settings)
        .map_err(|error| error.to_string())?;
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
    StreamError {
        reason: String,
    },
}

/// Translate the gateway's projection stream into UI events.
///
/// The stream reconnects itself, so this task ends only when the thread changes,
/// the app exits, or the stream fails terminally.
async fn pump_events(app: AppHandle, client: GatewayClient, thread_id: ThreadId) {
    let mut stream = client.events(thread_id);
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
            Ok(GatewayEvent::CapabilityActivity(activity)) => ChatEvent::Activity {
                capability_id: activity.capability_id,
                status: activity.status,
            },
            Ok(GatewayEvent::Error(error)) => ChatEvent::StreamError {
                reason: format!("The agent's event stream failed ({}).", error.kind),
            },
            // `keep_alive`, previews, auth prompts, and unknown events are not
            // rendered by the Phase 2a widget.
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
async fn launch_local_model(job: Arc<ProcessJob>) -> Option<LocalLlm> {
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

    // The first model that isn't marked suspect. A richer selection (a
    // user-pinned default) lands with the model panel; until then, first usable
    // wins, deterministically ordered by the store.
    let Some(model) = installed.into_iter().find(|model| model.is_loadable()) else {
        tracing::info!("no installed local model; the gateway will start without one");
        return None;
    };

    tracing::info!(model = %model.id, "bringing up the local model");
    let options = LocalLlmOptions {
        on_sidecar_spawn: Some(enlist_in_job(job)),
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
) -> (Vec<(String, String)>, Option<LocalLlm>) {
    match selection {
        ProviderSelection::Local => {
            let local = launch_local_model(job).await;
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
        ProviderSelection::Cloud { id, model } => (cloud_provider_env(id, model.as_deref()), None),
    }
}

/// The environment that points the gateway at a cloud provider, or empty when it
/// cannot be built. Every empty-returning path is logged.
fn cloud_provider_env(id: &str, model: Option<&str>) -> Vec<(String, String)> {
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
        Ok(Some(key)) => provider.llm_env(&key, model).unwrap_or_default(),
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

    // The gateway reads `LLM_BASE_URL` once at startup and never re-reads it, so
    // the model must be up — and its proxy URL known — before the gateway spawns.
    let (llm_env, local) = resolve_provider(job.clone(), &selection).await;
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

    let started = async {
        let token = SecretStore::new()
            .gateway_token()
            .map_err(|error| error.to_string())?;
        let mut config = GatewayConfig::new(reborn_binary(), reborn_home()?, token)
            .map_err(|error| error.to_string())?;
        config.llm_env = llm_env;
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

            // Store *before* emitting. A UI that has not subscribed yet will miss
            // the event and fall back to reading `gateway_state`, and that read
            // must already see `Ready` — otherwise the widget waits forever for an
            // event that has been and gone.
            *app.state::<AppState>().gateway.lock().await = Some(gateway);
            let _ = app.emit("gateway://state", GatewayState::Ready);
            update_character(&app, |inputs| inputs.gateway = GatewayState::Ready).await;
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

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItemBuilder::with_id("show", "Show / hide widget").build(app)?;
    let dashboard = MenuItemBuilder::with_id("dashboard", "Open dashboard").build(app)?;
    let reset = MenuItemBuilder::with_id("reset", "Reset widget position").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
    let menu = MenuBuilder::new(app)
        .items(&[&show, &dashboard, &reset, &quit])
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
            "reset" => reset_widget_position(app),
            // Dropping the app drops the `ProcessJob`, which kills the gateway
            // and anything it spawned.
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
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

fn register_summon_hotkey(app: &AppHandle) {
    let result = app
        .global_shortcut()
        .on_shortcut(summon_shortcut(), |app, _shortcut, event| {
            // Fire on press only; a release would toggle straight back.
            if event.state() == ShortcutState::Pressed {
                toggle_widget(app);
            }
        });
    if let Err(error) = result {
        // Another application may already own Ctrl+Alt+Space. Not fatal.
        tracing::warn!(%error, "could not register the summon hotkey; use the tray");
    }
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

fn main() {
    init_logging();

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
            create_thread,
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
        ])
        .setup(|app| {
            let store = WindowStateStore::at(WindowStateStore::default_path()?);
            let window_state = store.load()?;
            let job = Arc::new(ProcessJob::new()?);

            app.manage(AppState {
                gateway: Mutex::new(None),
                job: Arc::clone(&job),
                pump: Mutex::new(None),
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
