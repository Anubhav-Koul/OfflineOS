//! Wiring the voice pipeline into the widget.
//!
//! `ic_voice` owns the audio and the models; this module is the glue that gives it
//! the three things it needs from the widget and nothing else:
//!
//! * a **gateway turn** — a spoken transcript is just a chat message, so [`start`]
//!   builds the pipeline's [`ReplyFn`](ic_voice::ReplyFn) here: send the transcript,
//!   wait for the run to finish, read the assistant's reply back off the timeline
//!   ([`drive_turn`]). Voice speaks on the **app's shared thread**, not one of its
//!   own: it is an alternate *input* to the same conversation, so a spoken question
//!   and a typed one belong to one transcript and the reply reaches the bubble.
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

/// How long Listening waits for a speech onset before giving up — a false wake or
/// an abandoned push-to-talk must not leave the character "listening" forever.
const LISTEN_TIMEOUT: Duration = Duration::from_secs(12);

/// Yields a gateway client when the gateway is ready, or `None` while it is still
/// starting. The widget implements this over its app state.
pub type ClientProvider =
    Arc<dyn Fn() -> futures_util::future::BoxFuture<'static, Option<GatewayClient>> + Send + Sync>;

/// Yields the conversation the app is showing, creating it if needed.
///
/// Voice used to open a thread of its own, which meant a spoken question and a
/// typed one were two different conversations: the character would answer out loud
/// something the dashboard had never heard of, and the reply could not surface in
/// the speech bubble because no window was watching that thread. Voice is an
/// *input* to the same conversation, not a channel of its own — so it takes the
/// app's shared thread.
pub type ThreadProvider =
    Arc<dyn Fn() -> futures_util::future::BoxFuture<'static, Option<ThreadId>> + Send + Sync>;

/// The microphone the user chose, read fresh on every open — so switching device
/// takes effect on the next unmute, not the next launch. `None` follows the OS
/// default.
pub type InputDeviceFn = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Whether a reply should be spoken, read fresh each turn.
///
/// The user can change `reply_mode` mid-conversation, and the answer must take
/// effect on the next reply rather than on the next app launch.
pub type SpeaksFn = Arc<dyn Fn() -> bool + Send + Sync>;

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
    thread_provider: ThreadProvider,
    speaks: SpeaksFn,
    input_device: InputDeviceFn,
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

    // The microphone factory: opens the user's chosen input on start / unmute /
    // device change, falling back to the OS default when they have not chosen one
    // (or their choice has been unplugged). Re-read per open, so switching device in
    // settings takes effect on the next unmute rather than the next launch.
    let capture_factory: ic_voice::CaptureFactory = {
        let device = Arc::clone(&input_device);
        Arc::new(move || {
            let chosen = device();
            CpalCapture::start_on(chosen.as_deref(), RING_SECONDS)
                .map(|c| Box::new(c) as Box<dyn Capture>)
        })
    };

    // The reply path: transcript → gateway turn → spoken reply, on voice's own
    // lazily-created thread. A send that fails (a wiped/lost thread after a
    // user-initiated data reset) drops the cached thread and retries once on a
    // fresh one, so voice recovers without an app restart.
    let reply: ic_voice::ReplyFn = {
        let provider = Arc::clone(&client_provider);
        let threads = Arc::clone(&thread_provider);
        let speaks = Arc::clone(&speaks);
        Arc::new(move |transcript: String| {
            let provider = Arc::clone(&provider);
            let threads = Arc::clone(&threads);
            let speaks = Arc::clone(&speaks);
            Box::pin(async move {
                let client = provider().await?;
                // The app's conversation, not one of voice's own — so the reply
                // reaches the speech bubble and the dashboard transcript like any
                // other, and a spoken question can be followed up by a typed one.
                let thread_id = threads().await?;
                let text = match drive_turn(&client, &thread_id, &transcript).await {
                    TurnResult::Reply(text) => text,
                    TurnResult::NothingToSpeak | TurnResult::SendFailed => return None,
                };
                // `read` means the user wants the answer on screen, not aloud. The
                // turn still ran and the bubble still shows it — this only decides
                // whether Piper is handed anything to say.
                if !speaks() {
                    tracing::debug!("reply mode is read-only; not speaking the reply");
                    return None;
                }
                Some(speechify(&text))
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
        listen_timeout: LISTEN_TIMEOUT,
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

/// How a spoken turn ended, distinguishing "the send itself failed" (retryable on
/// a fresh thread) from "sent, but nothing to speak" (final).
pub enum TurnResult {
    /// The turn completed with this reply text.
    Reply(String),
    /// The turn happened but produced nothing speakable (failed run, timeout,
    /// empty reply, or the thread was busy).
    NothingToSpeak,
    /// `send_message` itself failed — the thread may no longer exist.
    SendFailed,
}

/// Drive one full turn to completion and return the assistant's reply.
///
/// Sends the transcript, follows the run's status on the event stream until it is
/// terminal, then reads the latest assistant message off the timeline — the same
/// send → await-terminal → read-timeline dance the typed UI does, since the reply
/// text never rides the event stream itself.
pub async fn drive_turn(
    client: &GatewayClient,
    thread_id: &ThreadId,
    transcript: &str,
) -> TurnResult {
    use crate::gateway_client::SubmitOutcome;

    let outcome = match client
        .send_message(thread_id, transcript, &ClientActionId::new())
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(%error, "could not send the spoken transcript");
            return TurnResult::SendFailed;
        }
    };
    let run_id = match outcome {
        SubmitOutcome::Submitted { run_id } | SubmitOutcome::AlreadySubmitted { run_id } => run_id,
        // The thread is busy with a PREVIOUS run; our message was accepted but has
        // no run yet, and the `active_run_id` in this outcome is the old one.
        // Tracking it would speak the previous question's answer as if it were
        // ours — fail the turn instead (the reply will still land in the thread).
        SubmitOutcome::DeferredBusy { .. } => {
            tracing::info!("the voice thread is busy with an earlier turn; staying quiet");
            return TurnResult::NothingToSpeak;
        }
    };

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
            return TurnResult::NothingToSpeak;
        }
        Err(_) => {
            tracing::warn!("spoken turn timed out waiting for a reply");
            return TurnResult::NothingToSpeak;
        }
    }

    match client.timeline(thread_id, Some(5)).await {
        Ok(timeline) => timeline
            .latest_assistant_reply()
            .and_then(|message| message.content.clone())
            .filter(|text| !text.trim().is_empty())
            .map_or(TurnResult::NothingToSpeak, TurnResult::Reply),
        Err(error) => {
            tracing::warn!(%error, "could not read the spoken reply from the timeline");
            TurnResult::NothingToSpeak
        }
    }
}

/// Make an LLM reply listenable: drop the markup a voice should not read aloud.
///
/// Replies are markdown. Reading "asterisk asterisk" is worse than useless, and a
/// fenced code block spoken character-by-character is minutes of noise. This is a
/// light-touch text pass, not a markdown parser: fenced blocks become a short
/// notice, and inline emphasis/backticks/link syntax dissolve into their text.
pub fn speechify(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut in_fence = false;
    let mut skipped_code = false;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            if !in_fence {
                skipped_code = true;
            }
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if skipped_code {
            out.push_str("(code omitted) ");
            skipped_code = false;
        }
        // Headers and list bullets read fine as plain sentences.
        let line = trimmed
            .trim_start_matches('#')
            .trim_start_matches(['-', '*', '>'])
            .trim_start();
        let mut chars = line.chars().peekable();
        let mut link_text = false;
        while let Some(c) = chars.next() {
            match c {
                // Emphasis and inline code markers dissolve.
                '*' | '_' | '`' => {}
                // [text](url) → text: keep the bracket contents, skip the url.
                '[' => link_text = true,
                ']' => {
                    link_text = false;
                    if chars.peek() == Some(&'(') {
                        for next in chars.by_ref() {
                            if next == ')' {
                                break;
                            }
                        }
                    }
                }
                _ => out.push(c),
            }
        }
        let _ = link_text;
        out.push(' ');
    }
    if skipped_code {
        out.push_str("(code omitted)");
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::speechify;

    #[test]
    fn code_blocks_become_a_short_notice() {
        let reply = "Here you go:\n```rust\nfn main() {}\n```\nDone.";
        assert_eq!(speechify(reply), "Here you go: (code omitted) Done.");
    }

    #[test]
    fn emphasis_backticks_and_links_dissolve_into_their_text() {
        let reply = "**Bold** and _quiet_ with `inline` and [a link](https://example.com).";
        assert_eq!(speechify(reply), "Bold and quiet with inline and a link.");
    }

    #[test]
    fn headers_and_bullets_read_as_sentences() {
        let reply = "## Plan\n- First thing\n- Second thing";
        assert_eq!(speechify(reply), "Plan First thing Second thing");
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(speechify("It is noon."), "It is noon.");
    }

    #[test]
    fn an_unterminated_fence_still_notes_the_code() {
        let reply = "Look:\n```python\nprint('hi')";
        assert_eq!(speechify(reply), "Look: (code omitted)");
    }
}
