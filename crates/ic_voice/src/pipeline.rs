//! The driver: the async task that runs the whole voice loop.
//!
//! Everything else in this crate is a pure part ([`crate::session`],
//! [`crate::endpoint`], [`crate::envelope`]) or a stage behind a trait
//! ([`crate::stages`]). This module is the hands that move them: it drains the
//! capture ring on a timer, feeds the audio to the right stage for the current
//! state, and performs the [`VoiceEffect`](crate::session::VoiceEffect)s the state
//! machine returns.
//!
//! The shape of a turn, once running:
//!
//! 1. **Idle** — every drained batch goes to the wake word. A detection applies
//!    [`WakeDetected`](VoiceEvent::WakeDetected) and we start capturing.
//! 2. **Listening** — audio is accumulated as the utterance and, in parallel,
//!    scored frame-by-frame by the VAD into the [`Endpointer`]. Its `SpeechEnded`
//!    finalizes the utterance; no onset within the listen timeout gives up.
//! 3. **Transcribing / Sending / Speaking** — whisper, the gateway turn, and Piper
//!    each run in a **spawned stage task** that reports back through a channel, so
//!    the select loop (and mute/shutdown/device-change) stays responsive whatever
//!    the models are doing. Stage results carry a turn generation; anything from a
//!    superseded turn is discarded.
//! 4. While **Speaking**, barge-in is the wake phrase or the summon hotkey — NOT
//!    raw VAD, which would self-trigger on the character's own TTS coming back in
//!    through the microphone.
//!
//! Mute cuts the microphone (the capture stream is dropped, not just ignored) and
//! stops any playback; unmute brings capture back. The state machine
//! ([`VoiceSession`]) owns every transition decision, so this driver never invents
//! one — it only supplies events and carries out effects.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::capture::Capture;
use crate::endpoint::{EndpointConfig, EndpointEvent, Endpointer};
use crate::error::Result;
use crate::format::SAMPLE_RATE;
use crate::session::{VoiceEffect, VoiceEvent, VoiceSession, VoiceState};
use crate::stages::{AmplitudeSink, Playback, Player, Synthesizer, Transcriber, Vad, WakeWord};

/// How often the driver drains the capture ring and advances the loop. Small
/// enough to feel responsive, large enough to stay cheap.
const TICK: Duration = Duration::from_millis(20);

/// Makes a started microphone capture on demand. The driver calls it to (re)open
/// the mic on unmute and after a device change; a hard failure degrades to no
/// capture rather than killing the loop.
pub type CaptureFactory = Arc<dyn Fn() -> Result<Box<dyn Capture>> + Send + Sync>;

/// Sends a transcript to the gateway and resolves to the spoken reply, or `None`
/// when the turn failed or produced nothing to say. The widget implements this over
/// its `GatewayClient` (send → await terminal run → latest assistant reply).
pub type ReplyFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>>;
/// Reports what was transcribed, empty string included.
pub type TranscriptFn = Arc<dyn Fn(String) + Send + Sync>;

/// A callback from transcript to spoken reply. See [`ReplyFuture`].
pub type ReplyFn = Arc<dyn Fn(String) -> ReplyFuture + Send + Sync>;

/// Notified on every voice-state change, so the widget can drive the character
/// (listening / thinking / speaking) and the mic-live indicator.
pub type StateFn = Arc<dyn Fn(VoiceState) + Send + Sync>;

/// Everything the driver needs. The model-backed stages are trait objects so the
/// whole loop runs against fakes in tests.
pub struct PipelineConfig {
    /// Opens the microphone; called on start, unmute, and device change.
    pub capture_factory: CaptureFactory,
    /// Wake-word spotter — watched while idle, and *while speaking*, where a
    /// detection is barge-in. (Barge-in is deliberately not VAD-driven: the mic
    /// hears the character's own TTS through the speakers, and a VAD would
    /// self-trigger on that echo. The wake phrase is specific enough not to.)
    pub wake: Box<dyn WakeWord>,
    /// Voice-activity detector (utterance endpointing while listening).
    pub vad: Box<dyn Vad>,
    /// Utterance transcriber (run off the async runtime).
    pub transcriber: Box<dyn Transcriber>,
    /// Reply synthesizer (run off the async runtime).
    pub synthesizer: Box<dyn Synthesizer>,
    /// Speech playback.
    pub player: Arc<dyn Player>,
    /// Transcript → spoken reply.
    pub reply: ReplyFn,
    /// State-change notifier.
    pub on_state: StateFn,
    /// **What the microphone actually heard**, reported after every transcription —
    /// including an empty one.
    ///
    /// Without this the pipeline is a black box with five stages and one symptom
    /// ("nothing happened"): a muted mic, a deaf device, a wake word that never
    /// fired, a transcript whisper could not make out, and a gateway that failed all
    /// look identical from outside. An empty string means "I listened and heard
    /// nothing", which is a *different* and much more useful answer than silence.
    pub on_transcript: TranscriptFn,
    /// Lip-sync amplitude sink, handed to the player.
    pub amplitude: AmplitudeSink,
    /// Endpointing thresholds.
    pub endpoint: EndpointConfig,
    /// How long Listening waits for a speech onset before giving up (a false wake
    /// or an abandoned push-to-talk must not listen — and buffer audio — forever).
    pub listen_timeout: Duration,
    /// Start with the mic muted.
    pub start_muted: bool,
}

/// A control command to the running pipeline.
enum Command {
    /// Mute (`true`) or unmute (`false`) the microphone.
    SetMuted(bool),
    /// Manually start listening — the push-to-talk / summon-hotkey path. From Idle
    /// it acts as a wake detection; during Speaking it is barge-in.
    TriggerListen,
    /// The default input device changed (WASAPI): drop and reopen capture so we
    /// follow the new default mic. A no-op while muted (nothing is open).
    RestartCapture,
    /// Say something, whatever asked for it.
    ///
    /// Speech used to be reachable only from a *spoken* turn — the reply callback of
    /// a voice-initiated conversation. So a message the user **typed** was never
    /// spoken, no matter what their reply mode said, and the app had no way to talk
    /// to them at all unless they talked to it first.
    Speak(String),
    /// Stop the loop and release the microphone.
    Shutdown,
}

/// A handle to a running voice pipeline: toggle mute, or shut it down.
pub struct VoiceHandle {
    commands: mpsc::Sender<Command>,
    task: tokio::task::JoinHandle<()>,
}

impl VoiceHandle {
    /// Mute or unmute the microphone. Muting cuts capture and any playback.
    pub async fn set_muted(&self, muted: bool) {
        let _ = self.commands.send(Command::SetMuted(muted)).await;
    }

    /// Start listening now, as if the wake word had fired — the push-to-talk /
    /// summon-hotkey path. From Idle it starts a turn; while the character is
    /// speaking it is **barge-in** (stop playback, listen). A no-op in every
    /// other state (muted, or mid-turn).
    pub async fn trigger_listen(&self) {
        let _ = self.commands.send(Command::TriggerListen).await;
    }

    /// Tell the pipeline the default microphone changed, so it reopens capture on
    /// the new device. Driven by the WASAPI device-change watcher ([`crate::device`]).
    pub async fn restart_capture(&self) {
        let _ = self.commands.send(Command::RestartCapture).await;
    }

    /// A cloneable sender for the device-change watcher to nudge the pipeline from a
    /// COM callback thread without holding the handle.
    pub fn restart_trigger(&self) -> RestartTrigger {
        RestartTrigger {
            commands: self.commands.clone(),
        }
    }

    /// Say `text` aloud: synthesize it and play it, with the same lip-sync amplitude
    /// and barge-in behaviour as a spoken reply. Used for replies to *typed* messages.
    pub async fn speak(&self, text: String) {
        let _ = self.commands.send(Command::Speak(text)).await;
    }

    /// Stop the pipeline and wait for it to release the device.
    pub async fn shutdown(self) {
        let _ = self.commands.send(Command::Shutdown).await;
        let _ = self.task.await;
    }
}

/// A thread-safe, cloneable nudge to reopen capture — for the WASAPI device-change
/// watcher, whose COM callback runs on an arbitrary (non-async) thread. Uses a
/// non-blocking send: a dropped nudge (full channel) is harmless, since the next
/// device change will nudge again.
#[derive(Clone)]
pub struct RestartTrigger {
    commands: mpsc::Sender<Command>,
}

impl RestartTrigger {
    /// Ask the pipeline to reopen capture. Safe to call from any thread.
    pub fn fire(&self) {
        let _ = self.commands.try_send(Command::RestartCapture);
    }
}

/// Spawn the pipeline on the current tokio runtime and return a handle to it.
pub fn spawn(config: PipelineConfig) -> VoiceHandle {
    let (tx, rx) = mpsc::channel(8);
    let driver = Driver::new(config);
    let task = tokio::spawn(driver.run(rx));
    VoiceHandle { commands: tx, task }
}

/// The result of an off-loop stage task (transcription, the gateway turn, or
/// synthesis), tagged with the turn generation it belongs to so a stale task from
/// an interrupted turn can never inject its result into the next one.
enum StageOutcome {
    /// Whisper finished; empty means it heard nothing usable.
    Transcribed(u64, String),
    /// The gateway turn resolved (`None` = failed / nothing to speak).
    Reply(u64, Option<String>),
    /// Synthesis finished (or failed).
    Synthesized(u64, crate::error::Result<crate::stages::Speech>),
}

/// A hard cap on buffered utterance audio (~64 KB/s), as a belt over the listen
/// timeout: even a misbehaving endpointer cannot grow memory without bound.
const MAX_UTTERANCE_SAMPLES: usize = SAMPLE_RATE as usize * 60;

/// After a failed microphone open, wait this long before the next attempt, so a
/// missing device is retried (unlike a one-shot open) without hammering WASAPI.
const CAPTURE_RETRY: Duration = Duration::from_secs(3);

/// The owned state of the running loop.
struct Driver {
    session: VoiceSession,
    capture_factory: CaptureFactory,
    capture: Option<Box<dyn Capture>>,
    /// Do not retry a failed mic open before this instant.
    capture_backoff_until: Option<std::time::Instant>,
    wake: Box<dyn WakeWord>,
    vad: Box<dyn Vad>,
    endpointer: Endpointer,
    // Transcription and synthesis are blocking; shared so they can move onto a
    // blocking thread and back without reconstruction.
    transcriber: Arc<Mutex<Box<dyn Transcriber>>>,
    synthesizer: Arc<Mutex<Box<dyn Synthesizer>>>,
    player: Arc<dyn Player>,
    reply: ReplyFn,
    on_state: StateFn,
    on_transcript: TranscriptFn,
    amplitude: AmplitudeSink,
    playback: Option<Playback>,
    /// Where finished stage tasks report back into the select loop.
    stage_tx: mpsc::Sender<StageOutcome>,
    stage_rx: mpsc::Receiver<StageOutcome>,
    /// Bumped at every listening start; stage results from older turns are stale
    /// and discarded (e.g. a slow synthesis finishing after a barge-in).
    turn: u64,
    /// Audio accumulated since capture began, awaiting endpointing/transcription.
    utterance: Vec<f32>,
    /// Leftover samples that did not fill a whole VAD frame last tick.
    vad_buf: Vec<f32>,
    /// When Listening began, for the no-onset timeout.
    listening_since: Option<std::time::Instant>,
    listen_timeout: Duration,
    frame_ms: u32,
}

impl Driver {
    fn new(config: PipelineConfig) -> Self {
        let frame_ms = (config.vad.frame_size() as u32 * 1000 / SAMPLE_RATE).max(1);
        let mut session = VoiceSession::new();
        if config.start_muted {
            session.on(VoiceEvent::MuteChanged(true));
        }
        // Sized for the deepest in-flight backlog: one result per stage kind.
        let (stage_tx, stage_rx) = mpsc::channel(4);
        Self {
            session,
            capture_factory: config.capture_factory,
            capture: None,
            capture_backoff_until: None,
            wake: config.wake,
            vad: config.vad,
            endpointer: Endpointer::new(config.endpoint),
            transcriber: Arc::new(Mutex::new(config.transcriber)),
            synthesizer: Arc::new(Mutex::new(config.synthesizer)),
            player: config.player,
            reply: config.reply,
            on_state: config.on_state,
            on_transcript: config.on_transcript,
            amplitude: config.amplitude,
            playback: None,
            stage_tx,
            stage_rx,
            turn: 0,
            utterance: Vec::new(),
            vad_buf: Vec::new(),
            listening_since: None,
            listen_timeout: config.listen_timeout,
            frame_ms,
        }
    }

    async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        // Announce the initial state and open the mic if we start unmuted.
        (self.on_state)(self.session.state());
        self.sync_capture();
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            let wake = tokio::select! {
                biased;
                cmd = commands.recv() => Woke::Command(cmd),
                // Never yields None: the driver holds a stage_tx clone.
                Some(stage) = self.stage_rx.recv() => Woke::Stage(stage),
                // Only resolves while speaking; otherwise pends forever.
                () = wait_playback(&mut self.playback) => Woke::PlaybackFinished,
                _ = tick.tick() => Woke::Tick,
            };
            match wake {
                Woke::Command(None) | Woke::Command(Some(Command::Shutdown)) => break,
                Woke::Command(Some(Command::SetMuted(muted))) => self.set_muted(muted),
                Woke::Command(Some(Command::TriggerListen)) => match self.session.state() {
                    // Manual wake, mirroring the wake-word paths: from Idle it
                    // starts a turn; during playback it is barge-in — the user's
                    // deterministic interrupt (no VAD, so no echo self-trigger).
                    VoiceState::Idle => self.begin_listening(),
                    VoiceState::Speaking => self.barge_in(),
                    _ => {}
                },
                // The state machine decides: only Idle accepts it, so we never talk
                // over a user who is mid-utterance or waiting on an answer.
                // `step` only advances the state machine — the *effect* it returns is
                // what actually does the work, and dropping it means the transition
                // happens and nothing is spoken.
                Woke::Command(Some(Command::Speak(text))) => {
                    if let VoiceEffect::Speak(text) = self.step(VoiceEvent::SpeakRequested(text)) {
                        self.begin_synthesis(text);
                    }
                }
                Woke::Command(Some(Command::RestartCapture)) => {
                    // Drop the current mic and reopen the (new) default. Only while
                    // unmuted; sync_capture reopens via the factory.
                    if let Some(capture) = self.capture.take() {
                        capture.stop();
                    }
                    self.vad_buf.clear();
                    self.capture_backoff_until = None;
                    self.sync_capture();
                }
                Woke::Stage(stage) => self.on_stage(stage),
                Woke::PlaybackFinished => {
                    self.playback = None;
                    self.step(VoiceEvent::SpeakingEnded);
                }
                Woke::Tick => self.on_tick(),
            }
        }

        // Leaving: stop any playback and drop the mic.
        if let Some(playback) = self.playback.take() {
            playback.stop();
        }
        self.capture = None;
    }

    /// Drain the mic and route the audio according to the current state.
    ///
    /// Deliberately synchronous: everything slow (whisper, the gateway, Piper) runs
    /// in spawned stage tasks that report back through `stage_rx`, so the select
    /// loop — and with it mute, shutdown, and device changes — stays responsive in
    /// every state.
    fn on_tick(&mut self) {
        self.maintain_capture();
        let Some(capture) = self.capture.as_ref() else {
            return; // muted, or no usable microphone right now
        };
        let samples = capture.ring().drain();

        // The listen timeout must fire even on a silent ring (a dead-quiet mic
        // delivers samples, but a *failed* one delivers none — either way a wake
        // with no speech must not listen forever).
        if self.session.state() == VoiceState::Listening {
            let timed_out = !self.endpointer.is_speaking()
                && self
                    .listening_since
                    .is_some_and(|since| since.elapsed() >= self.listen_timeout);
            if timed_out {
                tracing::debug!("no speech after the wake; giving up listening");
                self.step(VoiceEvent::ListenTimeout);
                self.utterance.clear();
                self.vad_buf.clear();
                self.endpointer.reset();
                self.listening_since = None;
                return;
            }
        }

        if samples.is_empty() {
            return;
        }
        match self.session.state() {
            VoiceState::Idle => {
                if self.wake.process(&samples) {
                    self.wake.reset();
                    self.begin_listening();
                }
            }
            VoiceState::Listening => {
                // Cap the buffered utterance as a belt over the listen timeout —
                // memory must stay bounded even if endpointing misbehaves.
                let room = MAX_UTTERANCE_SAMPLES.saturating_sub(self.utterance.len());
                self.utterance
                    .extend_from_slice(&samples[..samples.len().min(room)]);
                // Act on the FIRST utterance end in this batch: a long batch can
                // contain both an end and a fresh onset, and taking only the last
                // event would silently merge two utterances.
                if self
                    .endpoint_frames(&samples)
                    .contains(&EndpointEvent::SpeechEnded)
                {
                    self.begin_transcription();
                }
            }
            // Barge-in = the wake phrase during playback. Deliberately not the
            // VAD: the mic hears the TTS itself through the speakers, and a
            // VAD-driven interrupt self-triggers on that echo. The summon
            // hotkey (TriggerListen) is the other, always-available interrupt.
            VoiceState::Speaking if self.wake.process(&samples) => {
                self.wake.reset();
                self.barge_in();
            }
            // Transcribing / Sending / Muted: mic audio is drained and discarded.
            _ => {}
        }
    }

    /// Feed `samples` to the VAD in whole frames and return every endpoint event
    /// they produced, in order. Leftover partial frames carry to the next tick.
    fn endpoint_frames(&mut self, samples: &[f32]) -> Vec<EndpointEvent> {
        self.vad_buf.extend_from_slice(samples);
        let frame = self.vad.frame_size();
        let mut events = Vec::new();
        while self.vad_buf.len() >= frame {
            let chunk: Vec<f32> = self.vad_buf.drain(..frame).collect();
            let prob = self.vad.predict(&chunk);
            if let Some(event) = self.endpointer.push(prob, self.frame_ms) {
                events.push(event);
            }
        }
        events
    }

    fn begin_listening(&mut self) {
        self.turn = self.turn.wrapping_add(1);
        self.step(VoiceEvent::WakeDetected);
        self.utterance.clear();
        self.vad_buf.clear();
        self.endpointer.reset();
        self.listening_since = Some(std::time::Instant::now());
    }

    /// The utterance ended: hand it to whisper on a blocking thread and return
    /// immediately. The result comes back as [`StageOutcome::Transcribed`].
    fn begin_transcription(&mut self) {
        // SpeechEnded → BeginTranscription.
        self.step(VoiceEvent::SpeechEnded);
        let utterance = std::mem::take(&mut self.utterance);
        // Trim the silence, *then* lift the level. Whisper invents filler to explain
        // leading silence ("No, what's 2 plus 2?" for a clip that opened with a
        // pause), and it mishears quiet speech — so give it neither.
        let mut utterance = crate::format::trim_silence(&utterance).to_vec();
        let gain = crate::format::normalize(&mut utterance);
        if gain > 1.0 {
            tracing::debug!(gain, "amplified a quiet utterance for transcription");
        }
        self.vad_buf.clear();
        self.listening_since = None;

        let transcriber = Arc::clone(&self.transcriber);
        let tx = self.stage_tx.clone();
        let turn = self.turn;
        let on_transcript = Arc::clone(&self.on_transcript);
        tokio::spawn(async move {
            let text = match tokio::task::spawn_blocking(move || {
                // Recover a poisoned lock: whisper builds a fresh state per call,
                // so one panicked transcription must not disable STT forever.
                let mut transcriber = transcriber.lock().unwrap_or_else(|p| p.into_inner());
                transcriber.transcribe(&utterance)
            })
            .await
            {
                Ok(Ok(text)) => text,
                Ok(Err(error)) => {
                    tracing::warn!(%error, "transcription failed");
                    String::new()
                }
                Err(error) => {
                    tracing::warn!(%error, "the transcription task panicked");
                    String::new()
                }
            };
            on_transcript(text.clone());
            let _ = tx.send(StageOutcome::Transcribed(turn, text)).await;
        });
    }

    /// A stage task finished: advance the state machine and launch the next stage.
    fn on_stage(&mut self, stage: StageOutcome) {
        let (turn, next) = match stage {
            StageOutcome::Transcribed(turn, text) => (turn, Some(VoiceEvent::Transcribed(text))),
            StageOutcome::Reply(turn, Some(text)) => (turn, Some(VoiceEvent::ReplyReceived(text))),
            StageOutcome::Reply(turn, None) => (turn, Some(VoiceEvent::ReplyFailed)),
            StageOutcome::Synthesized(turn, result) => {
                self.on_synthesized(turn, result);
                (turn, None)
            }
        };
        if turn != self.turn {
            // A stage from an interrupted turn (barge-in / mute+re-wake). The state
            // machine would also reject most of these, but the generation check
            // guarantees a slow stale synthesis can never speak into a new turn.
            tracing::debug!("discarding a stage result from a superseded turn");
            return;
        }
        let Some(event) = next else { return };
        match self.step(event) {
            VoiceEffect::Send(transcript) => self.begin_reply(transcript),
            VoiceEffect::Speak(text) => self.begin_synthesis(text),
            _ => {}
        }
    }

    /// Launch the gateway turn; the result comes back as [`StageOutcome::Reply`].
    fn begin_reply(&mut self, transcript: String) {
        let reply = Arc::clone(&self.reply);
        let tx = self.stage_tx.clone();
        let turn = self.turn;
        tokio::spawn(async move {
            let result = reply(transcript).await;
            let _ = tx.send(StageOutcome::Reply(turn, result)).await;
        });
    }

    /// Launch synthesis; the result comes back as [`StageOutcome::Synthesized`].
    fn begin_synthesis(&mut self, text: String) {
        tracing::debug!(chars = text.len(), "synthesizing speech");
        let synthesizer = Arc::clone(&self.synthesizer);
        let tx = self.stage_tx.clone();
        let turn = self.turn;
        tokio::spawn(async move {
            let result = match tokio::task::spawn_blocking(move || {
                // Poison recovery, as for the transcriber: Piper spawns a fresh
                // process per utterance, so the boxed synthesizer holds no state a
                // panic could corrupt.
                let mut synthesizer = synthesizer.lock().unwrap_or_else(|p| p.into_inner());
                synthesizer.synthesize(&text)
            })
            .await
            {
                Ok(result) => result,
                Err(error) => Err(crate::error::Error::Tts(format!(
                    "the synthesis task panicked: {error}"
                ))),
            };
            let _ = tx.send(StageOutcome::Synthesized(turn, result)).await;
        });
    }

    /// Synthesis finished: start playback if this turn is still speaking.
    fn on_synthesized(&mut self, turn: u64, result: crate::error::Result<crate::stages::Speech>) {
        if turn != self.turn || self.session.state() != VoiceState::Speaking {
            // Muted or interrupted while Piper worked — nothing to play.
            return;
        }
        let speech = match result {
            Ok(speech) if !speech.is_empty() => speech,
            Ok(_) => {
                self.step(VoiceEvent::SpeakingEnded);
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "speech synthesis failed");
                self.step(VoiceEvent::SpeakingEnded);
                return;
            }
        };
        // Discard everything the mic heard while whisper and the gateway worked —
        // replaying that stale audio into the barge-in detector would fire on
        // speech from seconds ago.
        if let Some(capture) = self.capture.as_ref() {
            let _ = capture.ring().drain();
        }
        self.vad_buf.clear();
        self.wake.reset();
        match self.player.play(speech, Arc::clone(&self.amplitude)) {
            Ok(playback) => self.playback = Some(playback),
            Err(error) => {
                tracing::warn!(%error, "could not start playback");
                self.step(VoiceEvent::SpeakingEnded);
            }
        }
    }

    /// The user cut in while we were speaking (wake phrase or summon hotkey).
    fn barge_in(&mut self) {
        let effect = self.step(VoiceEvent::BargeIn);
        if effect == VoiceEffect::StopSpeaking
            && let Some(playback) = self.playback.take()
        {
            playback.stop();
        }
        // Now Listening: capture the barge-in utterance from here.
        self.turn = self.turn.wrapping_add(1);
        self.utterance.clear();
        self.vad_buf.clear();
        self.endpointer.reset();
        self.listening_since = Some(std::time::Instant::now());
    }

    /// Apply a mute change: stop playback if we were speaking, and open or close the
    /// microphone to match. The effect is evaluated *before* any playback is taken,
    /// so a redundant toggle can never orphan a live playback.
    fn set_muted(&mut self, muted: bool) {
        let effect = self.step(VoiceEvent::MuteChanged(muted));
        if effect == VoiceEffect::StopSpeaking
            && let Some(playback) = self.playback.take()
        {
            playback.stop();
        }
        self.sync_capture();
    }

    /// Apply an event to the state machine, announce a resulting state change, and
    /// return the effect for the caller to perform.
    fn step(&mut self, event: VoiceEvent) -> VoiceEffect {
        let before = self.session.state();
        let effect = self.session.on(event);
        let after = self.session.state();
        if after != before {
            tracing::debug!(?before, ?after, "voice state");
            (self.on_state)(after);
        }
        effect
    }

    /// Keep the microphone matched to the state on every tick: reopen after a
    /// transient failure (with a cooldown) and replace a stream that died without
    /// a device-change notification. A machine that grows a mic later just works.
    fn maintain_capture(&mut self) {
        if let Some(capture) = self.capture.as_ref()
            && !capture.is_healthy()
        {
            tracing::warn!("the capture stream died; reopening");
            if let Some(capture) = self.capture.take() {
                capture.stop();
            }
            self.vad_buf.clear();
        }
        let retry_ok = self
            .capture_backoff_until
            .is_none_or(|until| std::time::Instant::now() >= until);
        if self.capture.is_none() && self.session.state() != VoiceState::Muted && retry_ok {
            self.sync_capture();
        }
    }

    /// Bring capture into line with the state: the mic runs whenever we are not
    /// muted. Failing to open the mic degrades to no capture (retried by
    /// [`maintain_capture`] after [`CAPTURE_RETRY`]) rather than crashing.
    fn sync_capture(&mut self) {
        let want = self.session.state() != VoiceState::Muted;
        match (want, self.capture.is_some()) {
            (true, false) => match (self.capture_factory)() {
                Ok(capture) => {
                    self.capture = Some(capture);
                    self.capture_backoff_until = None;
                }
                Err(error) => {
                    tracing::warn!(%error, "could not open the microphone; will retry");
                    self.capture_backoff_until = Some(std::time::Instant::now() + CAPTURE_RETRY);
                }
            },
            (false, true) => {
                if let Some(capture) = self.capture.take() {
                    capture.stop();
                }
            }
            _ => {}
        }
    }
}

/// What woke the select loop.
enum Woke {
    Command(Option<Command>),
    Stage(StageOutcome),
    PlaybackFinished,
    Tick,
}

/// Resolve when the current playback finishes; pend forever when not speaking, so it
/// is a harmless arm in the select while idle.
async fn wait_playback(playback: &mut Option<Playback>) {
    match playback {
        Some(playback) => playback.finished().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::FakeCapture;
    use crate::ring::SampleRing;
    use crate::testsupport::{FakePlayer, FakeSynthesizer, FakeTranscriber, FakeVad, FakeWakeWord};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A frame of "speech" — non-zero, loud enough for the fake VAD.
    fn speech(samples: usize) -> Vec<f32> {
        vec![0.3; samples]
    }
    fn silence(samples: usize) -> Vec<f32> {
        vec![0.0; samples]
    }

    struct Harness {
        ring: SampleRing,
        wake_arm: Arc<AtomicBool>,
        synth: FakeSynthesizer,
        player: FakePlayer,
        states: Arc<Mutex<Vec<VoiceState>>>,
        sent: Arc<Mutex<Vec<String>>>,
        /// Everything the pipeline reported hearing, empty transcripts included.
        transcripts: Arc<Mutex<Vec<String>>>,
    }

    /// Knobs for the test pipeline; the defaults are the happy path.
    struct Options {
        reply: Option<&'static str>,
        transcript: &'static str,
        /// Artificial gateway latency, to test responsiveness while Sending.
        reply_delay: Duration,
        listen_timeout: Duration,
    }

    impl Default for Options {
        fn default() -> Self {
            Self {
                reply: Some("It is noon."),
                transcript: "what time is it",
                reply_delay: Duration::ZERO,
                listen_timeout: Duration::from_secs(10),
            }
        }
    }

    fn build_with(options: Options) -> (VoiceHandle, Harness) {
        let ring = SampleRing::for_seconds(30.0);
        let factory_ring = ring.clone();
        let capture_factory: CaptureFactory = Arc::new(move || {
            Ok(Box::new(FakeCapture::with_ring(factory_ring.clone())) as Box<dyn Capture>)
        });

        let wake = FakeWakeWord::new();
        let wake_arm = wake.control();
        let transcriber = FakeTranscriber::new(options.transcript);
        let synth = FakeSynthesizer::new(22_050, 4096);
        let player = FakePlayer::new();
        let states = Arc::new(Mutex::new(Vec::new()));
        let sent = Arc::new(Mutex::new(Vec::new()));

        let states_sink = Arc::clone(&states);
        let transcripts: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let transcripts_sink = Arc::clone(&transcripts);
        let sent_sink = Arc::clone(&sent);
        let reply = options.reply;
        let reply_delay = options.reply_delay;
        let reply_fn: ReplyFn = Arc::new(move |transcript: String| {
            if let Ok(mut s) = sent_sink.lock() {
                s.push(transcript);
            }
            let reply = reply.map(|r| r.to_string());
            Box::pin(async move {
                if !reply_delay.is_zero() {
                    tokio::time::sleep(reply_delay).await;
                }
                reply
            }) as ReplyFuture
        });

        let config = PipelineConfig {
            capture_factory,
            wake: Box::new(wake),
            vad: Box::new(FakeVad::new(512, 0.1)),
            transcriber: Box::new(transcriber.clone()),
            synthesizer: Box::new(synth.clone()),
            player: Arc::new(player.clone()),
            reply: reply_fn,
            on_state: Arc::new(move |state| {
                if let Ok(mut s) = states_sink.lock() {
                    s.push(state);
                }
            }),
            amplitude: crate::stages::null_amplitude(),
            on_transcript: {
                let sink = Arc::clone(&transcripts_sink);
                Arc::new(move |text: String| {
                    if let Ok(mut heard) = sink.lock() {
                        heard.push(text);
                    }
                })
            },
            endpoint: EndpointConfig {
                // Small thresholds so a short scripted clip endpoints quickly.
                min_speech_ms: 32,
                hangover_ms: 96,
                max_utterance_ms: 5_000,
                ..EndpointConfig::default()
            },
            listen_timeout: options.listen_timeout,
            start_muted: false,
        };

        let handle = spawn(config);
        (
            handle,
            Harness {
                ring,
                wake_arm,
                synth,
                player,
                states,
                sent,
                transcripts,
            },
        )
    }

    fn build(reply: Option<&'static str>) -> (VoiceHandle, Harness) {
        build_with(Options {
            reply,
            ..Options::default()
        })
    }

    /// Wake, speak, and fall silent so the utterance endpoints — the front half of
    /// every turn.
    async fn speak_an_utterance(h: &Harness) {
        h.wake_arm.store(true, Ordering::SeqCst);
        h.ring.write(&speech(512));
        settle().await;
        for _ in 0..6 {
            h.ring.write(&speech(512));
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        for _ in 0..8 {
            h.ring.write(&silence(512));
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        settle().await;
    }

    async fn settle() {
        // Let the 20 ms tick loop run several times.
        tokio::time::sleep(Duration::from_millis(120)).await;
    }

    /// A reply to a **typed** message must be spoken.
    ///
    /// The regression, in the user's words: "I still can't hear from the app."
    /// `Speaking` was reachable only from `Sending` — the tail of a conversation the
    /// user had started *with their voice*. So an app configured to speak its replies
    /// stayed silent for every typed message, which is most of them, and there was no
    /// way to make it talk except to talk to it first.
    #[tokio::test]
    async fn a_typed_reply_is_spoken() {
        let (handle, h) = build(None);

        handle.speak("Hello Anubhav.".to_string()).await;
        settle().await;
        settle().await;

        assert_eq!(
            h.synth.spoken(),
            ["Hello Anubhav."],
            "nothing was synthesized"
        );
        assert_eq!(h.player.played().len(), 1, "nothing was played");
        let states = h.states.lock().unwrap().clone();
        assert!(states.contains(&VoiceState::Speaking), "{states:?}");
        handle.shutdown().await;
    }

    /// The pipeline must say what it heard — and say so even when it heard nothing.
    ///
    /// The regression: a user spoke, nothing happened, and there was no way to tell
    /// a muted microphone from a deaf device from a wake word that never fired from a
    /// transcript whisper could not make out. All five look identical from outside.
    /// An empty transcript reported *as* an empty transcript is the difference
    /// between "I heard nothing" and silence.
    #[tokio::test]
    async fn the_pipeline_reports_what_it_heard_including_nothing() {
        let (handle, h) = build(Some("It is noon."));

        h.wake_arm.store(true, Ordering::SeqCst);
        h.ring.write(&speech(512));
        settle().await;
        for _ in 0..6 {
            h.ring.write(&speech(512));
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        for _ in 0..8 {
            h.ring.write(&silence(512));
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        settle().await;
        settle().await;

        let heard = h.transcripts.lock().unwrap().clone();
        assert_eq!(
            heard,
            ["what time is it"],
            "the transcript was not reported"
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn a_full_spoken_turn_flows_from_wake_to_playback() {
        let (handle, h) = build(Some("It is noon."));

        // Wake, then speak an utterance, then fall silent so it endpoints.
        h.wake_arm.store(true, Ordering::SeqCst);
        h.ring.write(&speech(512)); // triggers wake this tick
        settle().await;
        for _ in 0..6 {
            h.ring.write(&speech(512));
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        for _ in 0..8 {
            h.ring.write(&silence(512));
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        settle().await;
        settle().await;

        // The transcript reached the gateway, and the reply was synthesized+played.
        assert_eq!(h.sent.lock().unwrap().as_slice(), ["what time is it"]);
        assert_eq!(h.synth.spoken(), ["It is noon."]);
        assert_eq!(h.player.played().len(), 1);
        // We passed through listening and speaking.
        let states = h.states.lock().unwrap().clone();
        assert!(states.contains(&VoiceState::Listening), "{states:?}");
        assert!(states.contains(&VoiceState::Speaking), "{states:?}");

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn an_empty_transcript_never_reaches_the_gateway() {
        // The transcriber hears only noise (whitespace transcript).
        let (handle, h) = build_with(Options {
            reply: Some("ignored"),
            transcript: "   ",
            ..Options::default()
        });
        speak_an_utterance(&h).await;
        settle().await;

        assert!(h.sent.lock().unwrap().is_empty(), "empty transcript sent");
        assert!(h.player.played().is_empty());
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn muting_stops_playback_and_closes_the_mic() {
        let (handle, h) = build(Some("A very long reply."));
        h.player.hold(); // keep "speaking" until stopped

        speak_an_utterance(&h).await;
        settle().await;

        // Should now be playing (held).
        assert_eq!(h.player.played().len(), 1);
        assert_eq!(h.player.stop_count(), 0);

        handle.set_muted(true).await;
        settle().await;
        // Mute cut the held playback.
        assert_eq!(h.player.stop_count(), 1);

        handle.shutdown().await;
    }

    /// Mute must take effect *while the gateway is still working* — the driver may
    /// not block its command channel on the reply — and the late reply must then
    /// be discarded rather than spoken while muted.
    #[tokio::test]
    async fn mute_is_instant_while_awaiting_the_gateway_and_the_late_reply_is_not_spoken() {
        let (handle, h) = build_with(Options {
            reply: Some("Too late."),
            reply_delay: Duration::from_millis(600),
            ..Options::default()
        });
        speak_an_utterance(&h).await;

        // The turn is now Sending (the reply resolves in ~600 ms). Mute NOW.
        let muted_at = std::time::Instant::now();
        handle.set_muted(true).await;
        settle().await;
        let states = h.states.lock().unwrap().clone();
        assert!(
            states.contains(&VoiceState::Muted),
            "mute was not applied while the gateway was pending: {states:?}"
        );
        assert!(
            muted_at.elapsed() < Duration::from_millis(400),
            "mute took longer than the pending reply"
        );

        // Let the delayed reply arrive: it lands in Muted and must not speak.
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(h.synth.spoken().is_empty(), "spoke a reply while muted");
        assert!(h.player.played().is_empty());

        handle.shutdown().await;
    }

    /// A wake with no speech after it must give up and return to idle, not listen
    /// (and buffer) forever.
    #[tokio::test]
    async fn listening_with_no_speech_times_out_back_to_idle() {
        let (handle, h) = build_with(Options {
            listen_timeout: Duration::from_millis(200),
            ..Options::default()
        });
        // Wake, then only silence.
        h.wake_arm.store(true, Ordering::SeqCst);
        h.ring.write(&speech(512)); // the wake batch itself
        settle().await;
        for _ in 0..12 {
            h.ring.write(&silence(512));
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        settle().await;

        let states = h.states.lock().unwrap().clone();
        assert!(states.contains(&VoiceState::Listening), "{states:?}");
        assert_eq!(
            states.last(),
            Some(&VoiceState::Idle),
            "should have timed out back to idle: {states:?}"
        );
        assert!(h.sent.lock().unwrap().is_empty(), "nothing should be sent");
        handle.shutdown().await;
    }

    /// The summon hotkey during playback is barge-in: stop speaking, start
    /// listening.
    #[tokio::test]
    async fn trigger_listen_during_playback_is_barge_in() {
        let (handle, h) = build(Some("A long story…"));
        h.player.hold();
        speak_an_utterance(&h).await;
        settle().await;
        assert_eq!(h.player.played().len(), 1);

        handle.trigger_listen().await;
        settle().await;
        assert_eq!(h.player.stop_count(), 1, "playback should stop");
        assert_eq!(
            h.states.lock().unwrap().last(),
            Some(&VoiceState::Listening),
            "barge-in should land in listening"
        );
        handle.shutdown().await;
    }

    /// The wake phrase during playback is also barge-in (the echo-safe interrupt —
    /// raw VAD would self-trigger on the character's own TTS through the
    /// speakers).
    #[tokio::test]
    async fn the_wake_word_during_playback_is_barge_in() {
        let (handle, h) = build(Some("A long story…"));
        h.player.hold();
        speak_an_utterance(&h).await;
        settle().await;
        assert_eq!(h.player.played().len(), 1);

        // The user says the wake phrase over the playback.
        h.wake_arm.store(true, Ordering::SeqCst);
        h.ring.write(&speech(512));
        settle().await;

        assert_eq!(h.player.stop_count(), 1, "playback should stop");
        assert_eq!(
            h.states.lock().unwrap().last(),
            Some(&VoiceState::Listening),
            "barge-in should land in listening"
        );
        handle.shutdown().await;
    }
}
