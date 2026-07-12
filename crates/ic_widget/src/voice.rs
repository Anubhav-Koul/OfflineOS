//! Wiring the voice pipeline into the widget.
//!
//! `ic_voice` owns the audio and the models; this module is the glue that gives it
//! the three things it needs from the widget and nothing else:
//!
//! * a **gateway turn** — a spoken transcript is just a chat message, so [`start`]
//!   builds the pipeline's [`ReplyFn`](ic_voice::ReplyFn) here: send the transcript,
//!   wait for the run to finish, read the assistant's reply back off the timeline
//!   ([`drive_turn`]). Voice keeps its own thread so a spoken conversation has
//!   continuity without entangling the typed chat.
//! * a **Job Object slot** — Piper's TTS subprocess is enlisted through
//!   [`enlist`] so a hard kill of the widget takes it down too, exactly like
//!   `llama-server` and the browser sidecar.
//! * **event callbacks** — the caller passes an `on_state`/`amplitude` pair that
//!   emit the Tauri events the character and mic indicator react to. This module
//!   stays Tauri-free (like [`crate::browser`]) so it is testable and the coupling
//!   lives in one place.
//!
//! Provisioning the models ([`ic_voice::VoiceAssets`]) can download hundreds of
//! megabytes on first run, so [`start`] is meant to be spawned in the background:
//! voice simply becomes available once it returns, and its absence degrades to "no
//! voice", never a failed launch.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ic_llama::download::Downloader;
use ic_voice::{
    AmplitudeSink, Capture, CpalCapture, DeviceWatcher, NullWakeWord, PiperTts, Player,
    RustpotterWake, SileroVad, StateFn, Synthesizer, Transcriber, VoiceAssets, VoiceHandle,
    WhisperStt, bundled_wake_models,
};

use crate::gateway_client::{ClientActionId, GatewayClient, GatewayEvent, ThreadId};
use crate::job_object::ProcessJob;

/// Seconds of audio the capture ring holds — enough for the wake-word window plus a
/// long utterance, small enough to stay cheap.
const RING_SECONDS: f32 = 12.0;

/// How long to wait for a spoken turn's reply before giving up and staying quiet.
const TURN_TIMEOUT: Duration = Duration::from_secs(120);

/// Yields a gateway client when the gateway is ready, or `None` while it is still
/// starting. The widget implements this over its app state.
pub type ClientProvider =
    Arc<dyn Fn() -> futures_util::future::BoxFuture<'static, Option<GatewayClient>> + Send + Sync>;

/// A running voice pipeline, held in app state for its lifetime.
pub struct VoiceService {
    handle: VoiceHandle,
    // Kept alive so device-change notifications keep arriving (it unregisters on
    // drop). `None` off Windows or if registration failed.
    _watcher: Option<DeviceWatcher>,
    muted: AtomicBool,
}

impl VoiceService {
    /// Whether the microphone is currently muted.
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::SeqCst)
    }

    /// Toggle mute and return the new state.
    pub async fn toggle_mute(&self) -> bool {
        let now = !self.muted.load(Ordering::SeqCst);
        self.muted.store(now, Ordering::SeqCst);
        self.handle.set_muted(now).await;
        now
    }

    /// Set mute to a specific value.
    pub async fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::SeqCst);
        self.handle.set_muted(muted).await;
    }

    /// Start listening now (the summon hotkey / push-to-talk path).
    pub async fn trigger_listen(&self) {
        self.handle.trigger_listen().await;
    }

    /// Stop the pipeline and release the microphone.
    pub async fn shutdown(self) {
        self.handle.shutdown().await;
    }
}

/// Build the enlist hook that puts Piper's TTS subprocess in the widget's job.
fn enlist(job: Arc<ProcessJob>) -> ic_voice::ChildEnlist {
    Arc::new(move |child: &std::process::Child| {
        job.assign_std(child)
            .map_err(|error| std::io::Error::other(error.to_string()))
    })
}

/// Provision the models, build the pipeline, and start it. Returns `None` (logged)
/// on any failure — a machine without a mic, or a download that could not complete,
/// simply has no voice.
///
/// `on_state` and `amplitude` are the caller's Tauri emitters; `client_provider`
/// yields a gateway client on demand so the reply path works the moment the gateway
/// is ready, even if voice started first.
// arch-exempt: too_many_args, one-shot wiring call; a param struct would be built
// and destructured only here, adding no clarity. See docs/desktop/voice-notes.md.
#[allow(clippy::too_many_arguments)]
pub async fn start(
    job: Arc<ProcessJob>,
    models_root: PathBuf,
    wake_dir: PathBuf,
    downloader: Downloader,
    client_provider: ClientProvider,
    on_state: StateFn,
    amplitude: AmplitudeSink,
    start_muted: bool,
) -> Option<VoiceService> {
    // Provision assets: use them in place if already present, else download.
    let assets = match VoiceAssets::locate(&models_root) {
        Some(assets) => assets,
        None => {
            tracing::info!("provisioning voice models (first run may download ~210 MB)");
            match VoiceAssets::ensure(&models_root, &downloader, None).await {
                Ok(assets) => assets,
                Err(error) => {
                    tracing::warn!(%error, "could not provision voice models; voice disabled");
                    return None;
                }
            }
        }
    };

    // Build the model-backed stages. Any failure disables voice rather than the app.
    let transcriber: Box<dyn Transcriber> = match WhisperStt::new(&assets.whisper_model) {
        Ok(stt) => Box::new(stt),
        Err(error) => {
            tracing::warn!(%error, "could not load the whisper model; voice disabled");
            return None;
        }
    };
    let vad = match SileroVad::new() {
        Ok(vad) => Box::new(vad),
        Err(error) => {
            tracing::warn!(%error, "could not initialise the VAD; voice disabled");
            return None;
        }
    };
    let synthesizer: Box<dyn Synthesizer> = Box::new(PiperTts::new(
        &assets.piper_exe,
        &assets.piper_voice,
        enlist(job),
    ));
    let player: Arc<dyn Player> = Arc::new(ic_voice::CpalPlayer::new());

    // Wake word from bundled reference models; none → push-to-talk only.
    let wake_models = bundled_wake_models(&wake_dir);
    let wake: Box<dyn ic_voice::WakeWord> = if wake_models.is_empty() {
        tracing::info!("no bundled wakeword models; voice uses the summon hotkey (push-to-talk)");
        Box::new(NullWakeWord)
    } else {
        match RustpotterWake::new(&wake_models) {
            Ok(spotter) => Box::new(spotter),
            Err(error) => {
                tracing::warn!(%error, "wakeword models failed to load; using push-to-talk");
                Box::new(NullWakeWord)
            }
        }
    };

    // The microphone factory: opens the current default input on start / unmute /
    // device change.
    let capture_factory: ic_voice::CaptureFactory =
        Arc::new(|| CpalCapture::start(RING_SECONDS).map(|c| Box::new(c) as Box<dyn Capture>));

    // The reply path: transcript → gateway turn → spoken reply, on voice's own
    // lazily-created thread.
    let thread: Arc<tokio::sync::Mutex<Option<ThreadId>>> = Arc::new(tokio::sync::Mutex::new(None));
    let reply: ic_voice::ReplyFn = {
        let provider = Arc::clone(&client_provider);
        Arc::new(move |transcript: String| {
            let provider = Arc::clone(&provider);
            let thread = Arc::clone(&thread);
            Box::pin(async move {
                let client = provider().await?;
                let thread_id = ensure_thread(&client, &thread).await?;
                drive_turn(&client, &thread_id, &transcript).await
            })
        })
    };

    let config = ic_voice::PipelineConfig {
        capture_factory,
        wake,
        vad,
        transcriber,
        synthesizer,
        player,
        reply,
        on_state,
        amplitude,
        endpoint: ic_voice::EndpointConfig::default(),
        start_muted,
    };

    let handle = ic_voice::spawn(config);

    // Follow the default mic when it changes. Best-effort: no watcher just means we
    // stay on whatever mic was default at start.
    let watcher = {
        let trigger = handle.restart_trigger();
        match DeviceWatcher::start(Arc::new(move || trigger.fire())) {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                tracing::warn!(%error, "device-change notifications unavailable");
                None
            }
        }
    };

    tracing::info!("voice pipeline started");
    Some(VoiceService {
        handle,
        _watcher: watcher,
        muted: AtomicBool::new(start_muted),
    })
}

/// Get voice's thread, creating it on first use.
async fn ensure_thread(
    client: &GatewayClient,
    thread: &tokio::sync::Mutex<Option<ThreadId>>,
) -> Option<ThreadId> {
    let mut guard = thread.lock().await;
    if let Some(id) = guard.as_ref() {
        return Some(id.clone());
    }
    match client.create_thread().await {
        Ok(id) => {
            *guard = Some(id.clone());
            Some(id)
        }
        Err(error) => {
            tracing::warn!(%error, "could not create the voice thread");
            None
        }
    }
}

/// Drive one full turn to completion and return the assistant's reply.
///
/// Sends the transcript, follows the run's status on the event stream until it is
/// terminal, then reads the latest assistant message off the timeline — the same
/// send → await-terminal → read-timeline dance the typed UI does, since the reply
/// text never rides the event stream itself. `None` on a failed run, a timeout, or
/// an empty reply.
pub async fn drive_turn(
    client: &GatewayClient,
    thread_id: &ThreadId,
    transcript: &str,
) -> Option<String> {
    let outcome = match client
        .send_message(thread_id, transcript, &ClientActionId::new())
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(%error, "could not send the spoken transcript");
            return None;
        }
    };
    let run_id = outcome.run_id().clone();

    let terminal_ok =
        tokio::time::timeout(TURN_TIMEOUT, async {
            let mut stream = client.events(thread_id.clone());
            while let Some(event) = stream.next().await {
                let Ok(
                    GatewayEvent::ProjectionSnapshot(state) | GatewayEvent::ProjectionUpdate(state),
                ) = event
                else {
                    continue;
                };
                if let Some(status) = state.run_phase(&run_id)
                    && status.status.is_terminal()
                {
                    // Completed is the only phase with a reply to speak.
                    return matches!(status.status, crate::gateway_client::RunPhase::Completed);
                }
            }
            false
        })
        .await;

    match terminal_ok {
        Ok(true) => {}
        Ok(false) => {
            tracing::info!("spoken turn ended without a reply to speak");
            return None;
        }
        Err(_) => {
            tracing::warn!("spoken turn timed out waiting for a reply");
            return None;
        }
    }

    match client.timeline(thread_id, Some(5)).await {
        Ok(timeline) => timeline
            .latest_assistant_reply()
            .and_then(|message| message.content.clone())
            .filter(|text| !text.trim().is_empty()),
        Err(error) => {
            tracing::warn!(%error, "could not read the spoken reply from the timeline");
            None
        }
    }
}
