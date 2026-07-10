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

use ic_widget::error::Error;
use ic_widget::gateway_client::{
    ClientActionId, GateRef, GateResolution, GatewayClient, GatewayEvent, ProjectionItem, RunId,
    ThreadId,
};
use ic_widget::supervisor::{GatewayConfig, GatewayState, GatewaySupervisor};
use ic_widget::window_state::{LayoutHash, MonitorInfo, WindowPosition};
use ic_widget::{ProcessJob, RunPhase, SecretStore, WindowState, WindowStateStore};
use serde::Serialize;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tokio::sync::Mutex;

const WIDGET: &str = "widget";
const DASHBOARD: &str = "dashboard";

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
    window_state: std::sync::Mutex<WindowState>,
    window_store: WindowStateStore,
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
    *pump = Some(tokio::spawn(pump_events(app, client, thread_id.clone())));

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

// ----------------------------------------------------------------- windows

fn build_widget(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    WebviewWindowBuilder::new(app, WIDGET, WebviewUrl::App("index.html".into()))
        .title("IronClaw")
        .inner_size(380.0, 540.0)
        .min_inner_size(320.0, 360.0)
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

/// Start the gateway off the main thread and mirror its state onto the UI.
///
/// A first boot runs migrations and installs bundled skills. Blocking the UI for
/// that would be a splash screen; a health badge is a better answer.
fn spawn_gateway(app: AppHandle, job: Arc<ProcessJob>) {
    tauri::async_runtime::spawn(async move {
        let started = async {
            let token = SecretStore::new()
                .gateway_token()
                .map_err(|error| error.to_string())?;
            let config = GatewayConfig::new(reborn_binary(), reborn_home()?, token)
                .map_err(|error| error.to_string())?;
            GatewaySupervisor::start(config, job)
                .await
                .map_err(|error| error.to_string())
        }
        .await;

        match started {
            Ok(gateway) => {
                tracing::info!(base_url = gateway.client().base_url(), "gateway is ready");

                // Mirror every later transition onto the UI.
                let mut states = gateway.subscribe();
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    while states.changed().await.is_ok() {
                        let current = states.borrow_and_update().clone();
                        tracing::info!(?current, "gateway state changed");
                        let _ = handle.emit("gateway://state", &current);
                    }
                });

                // Store *before* emitting. A UI that has not subscribed yet will
                // miss the event and fall back to reading `gateway_state`, and
                // that read must already see `Ready` — otherwise the widget waits
                // forever for an event that has been and gone.
                *app.state::<AppState>().gateway.lock().await = Some(gateway);
                let _ = app.emit("gateway://state", GatewayState::Ready);
            }
            Err(reason) => {
                tracing::error!(%reason, "the gateway did not start");
                let _ = app.emit("gateway://state", GatewayState::Unhealthy { reason });
            }
        }
    });
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
            list_threads,
            list_automations,
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
                window_state: std::sync::Mutex::new(window_state),
                window_store: store,
            });

            let handle = app.handle().clone();
            let widget = build_widget(&handle)?;
            restore_position(&widget, &app.state::<AppState>());
            widget.show()?;

            build_tray(&handle)?;
            register_summon_hotkey(&handle);
            spawn_gateway(handle.clone(), job);

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
