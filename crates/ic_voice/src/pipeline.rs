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
//!    finalizes the utterance.
//! 3. **Transcribing** — the utterance is transcribed off the runtime; a non-empty
//!    transcript is sent to the gateway via the [`ReplyFn`].
//! 4. **Speaking** — the reply is synthesized and played, while the VAD keeps
//!    watching for **barge-in** (the user's speech onset), which stops playback and
//!    returns to listening.
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
    /// Wake-word spotter (Idle).
    pub wake: Box<dyn WakeWord>,
    /// Voice-activity detector (endpointing + barge-in).
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
    /// Lip-sync amplitude sink, handed to the player.
    pub amplitude: AmplitudeSink,
    /// Endpointing thresholds.
    pub endpoint: EndpointConfig,
    /// Start with the mic muted.
    pub start_muted: bool,
}

/// A control command to the running pipeline.
enum Command {
    /// Mute (`true`) or unmute (`false`) the microphone.
    SetMuted(bool),
    /// Manually start listening — the push-to-talk / summon-hotkey path, equivalent
    /// to a wake-word detection. Ignored unless idle.
    TriggerListen,
    /// The default input device changed (WASAPI): drop and reopen capture so we
    /// follow the new default mic. A no-op while muted (nothing is open).
    RestartCapture,
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
    /// summon-hotkey path. A no-op unless the pipeline is idle (not muted, not
    /// already in a turn).
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

/// The owned state of the running loop.
struct Driver {
    session: VoiceSession,
    capture_factory: CaptureFactory,
    capture: Option<Box<dyn Capture>>,
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
    amplitude: AmplitudeSink,
    playback: Option<Playback>,
    /// Audio accumulated since capture began, awaiting endpointing/transcription.
    utterance: Vec<f32>,
    /// Leftover samples that did not fill a whole VAD frame last tick.
    vad_buf: Vec<f32>,
    frame_ms: u32,
}

impl Driver {
    fn new(config: PipelineConfig) -> Self {
        let frame_ms = (config.vad.frame_size() as u32 * 1000 / SAMPLE_RATE).max(1);
        let mut session = VoiceSession::new();
        if config.start_muted {
            session.on(VoiceEvent::MuteChanged(true));
        }
        Self {
            session,
            capture_factory: config.capture_factory,
            capture: None,
            wake: config.wake,
            vad: config.vad,
            endpointer: Endpointer::new(config.endpoint),
            transcriber: Arc::new(Mutex::new(config.transcriber)),
            synthesizer: Arc::new(Mutex::new(config.synthesizer)),
            player: config.player,
            reply: config.reply,
            on_state: config.on_state,
            amplitude: config.amplitude,
            playback: None,
            utterance: Vec::new(),
            vad_buf: Vec::new(),
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
                // Only resolves while speaking; otherwise pends forever.
                () = wait_playback(&mut self.playback) => Woke::PlaybackFinished,
                _ = tick.tick() => Woke::Tick,
            };
            match wake {
                Woke::Command(None) | Woke::Command(Some(Command::Shutdown)) => break,
                Woke::Command(Some(Command::SetMuted(muted))) => self.set_muted(muted),
                Woke::Command(Some(Command::TriggerListen)) => {
                    // Manual wake: only meaningful from Idle (mirrors the wake-word
                    // path, which the state machine also only honours from Idle).
                    if self.session.state() == VoiceState::Idle {
                        self.begin_listening();
                    }
                }
                Woke::Command(Some(Command::RestartCapture)) => {
                    // Drop the current mic and reopen the (new) default. Only while
                    // unmuted; sync_capture reopens via the factory.
                    if let Some(capture) = self.capture.take() {
                        capture.stop();
                    }
                    self.vad_buf.clear();
                    self.sync_capture();
                }
                Woke::PlaybackFinished => {
                    self.playback = None;
                    self.step(VoiceEvent::SpeakingEnded);
                    self.sync_capture();
                }
                Woke::Tick => self.on_tick().await,
            }
        }

        // Leaving: stop any playback and drop the mic.
        if let Some(playback) = self.playback.take() {
            playback.stop();
        }
        self.capture = None;
    }

    /// Drain the mic and route the audio according to the current state.
    async fn on_tick(&mut self) {
        let Some(capture) = self.capture.as_ref() else {
            return; // muted: no mic
        };
        let samples = capture.ring().drain();
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
                self.utterance.extend_from_slice(&samples);
                if let Some(EndpointEvent::SpeechEnded) = self.endpoint_frames(&samples) {
                    self.finish_utterance().await;
                }
            }
            VoiceState::Speaking => {
                // Watch for the user cutting in.
                if let Some(EndpointEvent::SpeechStarted) = self.endpoint_frames(&samples) {
                    self.barge_in();
                }
            }
            // Transcribing / Sending / Muted: nothing to do with mic audio.
            _ => {}
        }
    }

    /// Feed `samples` to the VAD in whole frames and return the last endpoint event
    /// they produced, if any. Leftover partial frames are carried to the next tick.
    fn endpoint_frames(&mut self, samples: &[f32]) -> Option<EndpointEvent> {
        self.vad_buf.extend_from_slice(samples);
        let frame = self.vad.frame_size();
        let mut last = None;
        while self.vad_buf.len() >= frame {
            let chunk: Vec<f32> = self.vad_buf.drain(..frame).collect();
            let prob = self.vad.predict(&chunk);
            if let Some(event) = self.endpointer.push(prob, self.frame_ms) {
                last = Some(event);
            }
        }
        last
    }

    fn begin_listening(&mut self) {
        self.step(VoiceEvent::WakeDetected);
        self.utterance.clear();
        self.vad_buf.clear();
        self.endpointer.reset();
    }

    /// The utterance ended: transcribe it, and on a real transcript, send it and
    /// speak the reply.
    async fn finish_utterance(&mut self) {
        // SpeechEnded → BeginTranscription.
        self.step(VoiceEvent::SpeechEnded);
        let utterance = std::mem::take(&mut self.utterance);
        self.vad_buf.clear();

        let transcriber = Arc::clone(&self.transcriber);
        let text = tokio::task::spawn_blocking(move || {
            transcriber
                .lock()
                .expect("transcriber lock")
                .transcribe(&utterance)
        })
        .await;
        let text = match text {
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

        // Transcribed → Send(text) (or back to Idle on an empty transcript).
        if let VoiceEffect::Send(transcript) = self.step(VoiceEvent::Transcribed(text)) {
            self.send_and_speak(transcript).await;
        }
        self.sync_capture();
    }

    /// Send the transcript to the gateway, then synthesize and play the reply.
    async fn send_and_speak(&mut self, transcript: String) {
        let reply = (self.reply)(transcript).await;
        let effect = match reply {
            Some(text) => self.step(VoiceEvent::ReplyReceived(text)),
            None => self.step(VoiceEvent::ReplyFailed),
        };
        if let VoiceEffect::Speak(text) = effect {
            self.speak(text).await;
        }
    }

    /// Synthesize `text` and start playing it. Playback completion is observed by
    /// the main loop's `wait_playback` arm.
    async fn speak(&mut self, text: String) {
        let synthesizer = Arc::clone(&self.synthesizer);
        let speech = tokio::task::spawn_blocking(move || {
            synthesizer
                .lock()
                .expect("synthesizer lock")
                .synthesize(&text)
        })
        .await;
        let speech = match speech {
            Ok(Ok(speech)) => speech,
            Ok(Err(error)) => {
                tracing::warn!(%error, "speech synthesis failed");
                self.step(VoiceEvent::SpeakingEnded);
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "the synthesis task panicked");
                self.step(VoiceEvent::SpeakingEnded);
                return;
            }
        };
        if speech.is_empty() {
            // Nothing to say — treat as finished immediately.
            self.step(VoiceEvent::SpeakingEnded);
            return;
        }
        match self.player.play(speech, Arc::clone(&self.amplitude)) {
            Ok(playback) => {
                self.playback = Some(playback);
                // Fresh endpointer for barge-in detection during playback.
                self.vad_buf.clear();
                self.endpointer.reset();
            }
            Err(error) => {
                tracing::warn!(%error, "could not start playback");
                self.step(VoiceEvent::SpeakingEnded);
            }
        }
    }

    /// The user cut in while we were speaking.
    fn barge_in(&mut self) {
        if let (VoiceEffect::StopSpeaking, Some(playback)) =
            (self.step(VoiceEvent::BargeIn), self.playback.take())
        {
            playback.stop();
        }
        // Now Listening: capture the barge-in utterance from here.
        self.utterance.clear();
        self.vad_buf.clear();
        self.endpointer.reset();
    }

    /// Apply a mute change: stop playback if we were speaking, and open or close the
    /// microphone to match.
    fn set_muted(&mut self, muted: bool) {
        if let (VoiceEffect::StopSpeaking, Some(playback)) = (
            self.step(VoiceEvent::MuteChanged(muted)),
            self.playback.take(),
        ) {
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
            (self.on_state)(after);
        }
        effect
    }

    /// Bring capture into line with the state: the mic runs whenever we are not
    /// muted. Failing to open the mic degrades to no capture rather than crashing.
    fn sync_capture(&mut self) {
        let want = self.session.state() != VoiceState::Muted;
        match (want, self.capture.is_some()) {
            (true, false) => match (self.capture_factory)() {
                Ok(capture) => self.capture = Some(capture),
                Err(error) => tracing::warn!(%error, "could not open the microphone"),
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
    }

    fn build(reply: Option<&'static str>) -> (VoiceHandle, Harness) {
        let ring = SampleRing::for_seconds(30.0);
        let factory_ring = ring.clone();
        let capture_factory: CaptureFactory = Arc::new(move || {
            Ok(Box::new(FakeCapture::with_ring(factory_ring.clone())) as Box<dyn Capture>)
        });

        let wake = FakeWakeWord::new();
        let wake_arm = wake.control();
        let transcriber = FakeTranscriber::new("what time is it");
        let synth = FakeSynthesizer::new(22_050, 4096);
        let player = FakePlayer::new();
        let states = Arc::new(Mutex::new(Vec::new()));
        let sent = Arc::new(Mutex::new(Vec::new()));

        let states_sink = Arc::clone(&states);
        let sent_sink = Arc::clone(&sent);
        let reply_fn: ReplyFn = Arc::new(move |transcript: String| {
            if let Ok(mut s) = sent_sink.lock() {
                s.push(transcript);
            }
            let reply = reply.map(|r| r.to_string());
            Box::pin(async move { reply }) as ReplyFuture
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
            endpoint: EndpointConfig {
                // Small thresholds so a short scripted clip endpoints quickly.
                min_speech_ms: 32,
                hangover_ms: 96,
                max_utterance_ms: 5_000,
                ..EndpointConfig::default()
            },
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
            },
        )
    }

    async fn settle() {
        // Let the 20 ms tick loop run several times.
        tokio::time::sleep(Duration::from_millis(120)).await;
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
        let (handle, h) = build_with_transcript(Some("ignored"), "   ");
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

        assert!(h.sent.lock().unwrap().is_empty(), "empty transcript sent");
        assert!(h.player.played().is_empty());
        handle.shutdown().await;
    }

    fn build_with_transcript(
        reply: Option<&'static str>,
        transcript: &'static str,
    ) -> (VoiceHandle, Harness) {
        // Same as build() but with a custom transcriber reply.
        let ring = SampleRing::for_seconds(30.0);
        let factory_ring = ring.clone();
        let capture_factory: CaptureFactory = Arc::new(move || {
            Ok(Box::new(FakeCapture::with_ring(factory_ring.clone())) as Box<dyn Capture>)
        });
        let wake = FakeWakeWord::new();
        let wake_arm = wake.control();
        let transcriber = FakeTranscriber::new(transcript);
        let synth = FakeSynthesizer::new(22_050, 4096);
        let player = FakePlayer::new();
        let states = Arc::new(Mutex::new(Vec::new()));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sent_sink = Arc::clone(&sent);
        let reply_fn: ReplyFn = Arc::new(move |transcript: String| {
            if let Ok(mut s) = sent_sink.lock() {
                s.push(transcript);
            }
            let reply = reply.map(|r| r.to_string());
            Box::pin(async move { reply }) as ReplyFuture
        });
        let config = PipelineConfig {
            capture_factory,
            wake: Box::new(wake),
            vad: Box::new(FakeVad::new(512, 0.1)),
            transcriber: Box::new(transcriber.clone()),
            synthesizer: Box::new(synth.clone()),
            player: Arc::new(player.clone()),
            reply: reply_fn,
            on_state: Arc::new(move |_| {}),
            amplitude: crate::stages::null_amplitude(),
            endpoint: EndpointConfig {
                min_speech_ms: 32,
                hangover_ms: 96,
                max_utterance_ms: 5_000,
                ..EndpointConfig::default()
            },
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
            },
        )
    }

    #[tokio::test]
    async fn muting_stops_playback_and_closes_the_mic() {
        let (handle, h) = build(Some("A very long reply."));
        h.player.hold(); // keep "speaking" until stopped

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

        // Should now be playing (held).
        assert_eq!(h.player.played().len(), 1);
        assert_eq!(h.player.stop_count(), 0);

        handle.set_muted(true).await;
        settle().await;
        // Mute cut the held playback.
        assert_eq!(h.player.stop_count(), 1);

        handle.shutdown().await;
    }
}
