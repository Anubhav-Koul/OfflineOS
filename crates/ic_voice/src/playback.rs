//! Playing synthesized speech to the speakers, with a lip-sync amplitude tap.
//!
//! Piper renders mono at 22.05 kHz; the sound card wants its own rate and channel
//! count. [`CpalPlayer`] resamples the clip up to the device rate ([`Resampler`]),
//! opens the default output device, and streams the audio, upmixing mono to however
//! many channels the device has by duplicating the sample across a frame.
//!
//! Two things run alongside the audio:
//!
//! * a **lip-sync tap** — a companion thread walks the same mono samples in ~30 ms
//!   windows, paced by the wall clock to track playback, computes an RMS envelope
//!   ([`EnvelopeFollower`]) and hands it to the [`AmplitudeSink`]. It runs *off* the
//!   audio callback deliberately: the sink emits a Tauri event, which must never
//!   happen on the real-time audio thread.
//! * a **stop signal** — an atomic the audio callback and the companion thread both
//!   watch. Setting it (barge-in or mute) silences the stream immediately and ends
//!   the thread, which drops the stream and reports completion.
//!
//! The cpal `Stream` is not `Send`, so it is built and owned entirely inside one
//! dedicated thread; only plain data (the resampled samples, the device, atomics)
//! crosses the thread boundary.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::envelope::EnvelopeFollower;
use crate::error::{Error, Result};
use crate::resample::Resampler;
use crate::stages::{AmplitudeSink, Playback, Player, Speech};

/// How often the lip-sync tap emits an amplitude, and the RMS window it uses.
const TAP_INTERVAL: Duration = Duration::from_millis(30);

/// cpal-backed playback to the default output device.
pub struct CpalPlayer {
    host: cpal::Host,
}

impl Default for CpalPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl CpalPlayer {
    /// A player targeting the default output device of the default host.
    pub fn new() -> Self {
        Self {
            host: cpal::default_host(),
        }
    }
}

impl Player for CpalPlayer {
    fn play(&self, speech: Speech, amplitude: AmplitudeSink) -> Result<Playback> {
        let device = self
            .host
            .default_output_device()
            .ok_or_else(|| Error::NoDevice("no default output device".into()))?;
        let config = device
            .default_output_config()
            .map_err(|error| Error::audio(format!("reading the output config: {error}")))?;
        let device_rate = config.sample_rate();
        let channels = config.channels() as usize;
        let sample_format = config.sample_format();

        // Resample the whole clip up to the device rate on this (Send) thread, then
        // hand the plain sample buffer to the playback thread.
        let render = Arc::new(resample_to(&speech, device_rate)?);

        let stopped = Arc::new(AtomicBool::new(false));
        let played = Arc::new(AtomicUsize::new(0)); // mono samples consumed by the callback
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();

        let stopped_ctl = Arc::clone(&stopped);
        let thread_render = Arc::clone(&render);
        let thread_stopped = Arc::clone(&stopped);
        let thread_played = Arc::clone(&played);
        let stream_config: cpal::StreamConfig = config.into();

        // The cpal Stream is !Send: build and own it inside this one thread.
        std::thread::spawn(move || {
            let stream = build_output_stream(
                &device,
                &stream_config,
                sample_format,
                channels,
                Arc::clone(&thread_render),
                Arc::clone(&thread_played),
                Arc::clone(&thread_stopped),
            );
            let stream = match stream {
                Ok(stream) => stream,
                Err(error) => {
                    tracing::warn!(%error, "could not open the output stream");
                    let _ = finished_tx.send(());
                    return;
                }
            };
            if let Err(error) = stream.play() {
                tracing::warn!(%error, "could not start the output stream");
                let _ = finished_tx.send(());
                return;
            }

            run_lip_sync_tap(
                &thread_render,
                device_rate,
                &thread_played,
                &thread_stopped,
                &amplitude,
            );

            // Dropping the stream stops the device.
            drop(stream);
            let _ = finished_tx.send(());
        });

        Ok(Playback::new(
            move || stopped_ctl.store(true, Ordering::SeqCst),
            finished_rx,
        ))
    }
}

/// Resample a clip to `device_rate`, flushing the tail so nothing is dropped.
fn resample_to(speech: &Speech, device_rate: u32) -> Result<Vec<f32>> {
    if speech.is_empty() {
        return Ok(Vec::new());
    }
    let mut resampler = Resampler::to_rate(speech.sample_rate, device_rate)?;
    let mut out = resampler.push(&speech.samples);
    out.extend(resampler.flush());
    Ok(out)
}

/// The lip-sync tap: walk the mono render in ~30 ms windows paced by the wall clock,
/// emit a smoothed RMS to the sink, then decay the mouth shut at the end. Stops when
/// the audio finishes or the stop signal fires.
fn run_lip_sync_tap(
    render: &[f32],
    device_rate: u32,
    played: &AtomicUsize,
    stopped: &AtomicBool,
    amplitude: &AmplitudeSink,
) {
    let window = (device_rate as f32 * TAP_INTERVAL.as_secs_f32()).max(1.0) as usize;
    let mut follower = EnvelopeFollower::new();
    let start = Instant::now();
    let mut next_tick = TAP_INTERVAL;

    loop {
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        // Track playback position by wall-clock time so the mouth matches the ear.
        let elapsed = start.elapsed();
        let pos = (elapsed.as_secs_f32() * device_rate as f32) as usize;
        if pos >= render.len() {
            break;
        }
        let end = (pos + window).min(render.len());
        let level = follower.push(&render[pos..end]);
        amplitude(level);

        // Also stop promptly if the callback has drained everything.
        if played.load(Ordering::SeqCst) >= render.len() {
            break;
        }

        // Sleep to the next tick relative to start, resisting drift.
        let now = start.elapsed();
        if next_tick > now {
            std::thread::sleep(next_tick - now);
        }
        next_tick += TAP_INTERVAL;
    }

    // Close the mouth gently regardless of how playback ended.
    for _ in 0..4 {
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        amplitude(follower.decay());
        std::thread::sleep(TAP_INTERVAL);
    }
    amplitude(0.0);
}

/// Build the output stream for the device's sample format. The callback copies mono
/// samples from `render` (advancing `played`) into every channel of each frame, and
/// writes silence once drained or when `stopped`.
fn build_output_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    channels: usize,
    render: Arc<Vec<f32>>,
    played: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
) -> Result<cpal::Stream> {
    let err = |error| tracing::warn!(%error, "the output stream errored");
    let position = Arc::new(Mutex::new(0usize));

    let stream = match sample_format {
        cpal::SampleFormat::F32 => {
            let render = Arc::clone(&render);
            let played = Arc::clone(&played);
            let stopped = Arc::clone(&stopped);
            let position = Arc::clone(&position);
            device.build_output_stream(
                *config,
                move |out: &mut [f32], _: &_| {
                    fill(out, channels, &render, &position, &played, &stopped, |s| s);
                },
                err,
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let render = Arc::clone(&render);
            let played = Arc::clone(&played);
            let stopped = Arc::clone(&stopped);
            let position = Arc::clone(&position);
            device.build_output_stream(
                *config,
                move |out: &mut [i16], _: &_| {
                    fill(out, channels, &render, &position, &played, &stopped, |s| {
                        (s.clamp(-1.0, 1.0) * 32767.0) as i16
                    });
                },
                err,
                None,
            )
        }
        other => {
            return Err(Error::audio(format!(
                "unsupported output sample format: {other:?}"
            )));
        }
    }
    .map_err(|error| Error::audio(format!("building the output stream: {error}")))?;
    Ok(stream)
}

/// Fill one output buffer: mono `render` upmixed to `channels`, converted per
/// sample by `convert`. Writes silence when stopped or drained.
fn fill<S: Copy>(
    out: &mut [S],
    channels: usize,
    render: &[f32],
    position: &Mutex<usize>,
    played: &AtomicUsize,
    stopped: &AtomicBool,
    convert: impl Fn(f32) -> S,
) {
    let silence = convert(0.0);
    if stopped.load(Ordering::SeqCst) {
        out.fill(silence);
        return;
    }
    let mut pos = position.lock().unwrap_or_else(|p| p.into_inner());
    for frame in out.chunks_mut(channels.max(1)) {
        let sample = render.get(*pos).copied();
        match sample {
            Some(value) => {
                let converted = convert(value);
                frame.fill(converted);
                *pos += 1;
            }
            None => frame.fill(silence),
        }
    }
    played.store((*pos).min(render.len()), Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_upmixes_mono_to_stereo_and_advances_position() {
        let render = vec![0.5, -0.5];
        let position = Mutex::new(0usize);
        let played = AtomicUsize::new(0);
        let stopped = AtomicBool::new(false);
        let mut out = [0.0f32; 4]; // 2 stereo frames
        fill(&mut out, 2, &render, &position, &played, &stopped, |s| s);
        assert_eq!(out, [0.5, 0.5, -0.5, -0.5]);
        assert_eq!(played.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn fill_writes_silence_past_the_end_of_the_clip() {
        let render = vec![0.9];
        let position = Mutex::new(0usize);
        let played = AtomicUsize::new(0);
        let stopped = AtomicBool::new(false);
        let mut out = [1.0f32; 3]; // mono, one real sample then silence
        fill(&mut out, 1, &render, &position, &played, &stopped, |s| s);
        assert_eq!(out, [0.9, 0.0, 0.0]);
    }

    #[test]
    fn fill_is_all_silence_once_stopped() {
        let render = vec![0.9, 0.8, 0.7];
        let position = Mutex::new(0usize);
        let played = AtomicUsize::new(0);
        let stopped = AtomicBool::new(true);
        let mut out = [1.0f32; 3];
        fill(&mut out, 1, &render, &position, &played, &stopped, |s| s);
        assert_eq!(out, [0.0, 0.0, 0.0]);
    }

    #[test]
    fn resample_to_the_same_rate_is_a_passthrough() {
        let speech = Speech {
            samples: vec![0.1, 0.2, 0.3],
            sample_rate: 48_000,
        };
        assert_eq!(resample_to(&speech, 48_000).unwrap(), [0.1, 0.2, 0.3]);
    }

    #[test]
    fn resample_empty_speech_is_empty() {
        let speech = Speech {
            samples: vec![],
            sample_rate: 22_050,
        };
        assert!(resample_to(&speech, 48_000).unwrap().is_empty());
    }

    /// Real playback needs an output device. Ignored on CI.
    #[test]
    #[ignore = "needs a real speaker; run with --ignored"]
    fn plays_a_tone_and_reports_amplitude() {
        use std::sync::atomic::AtomicU32;
        // A 0.4 s 440 Hz tone at 22.05 kHz.
        let rate = 22_050u32;
        let samples: Vec<f32> = (0..(rate as usize * 2 / 5))
            .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.3)
            .collect();
        let ticks = Arc::new(AtomicU32::new(0));
        let ticks_sink = Arc::clone(&ticks);
        let sink: AmplitudeSink = Arc::new(move |level| {
            assert!((0.0..=1.0).contains(&level));
            ticks_sink.fetch_add(1, Ordering::SeqCst);
        });
        let player = CpalPlayer::new();
        let mut playback = player
            .play(
                Speech {
                    samples,
                    sample_rate: rate,
                },
                sink,
            )
            .expect("play");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(3), playback.finished())
                .await
                .expect("playback should finish");
        });
        assert!(
            ticks.load(Ordering::SeqCst) > 3,
            "amplitude tap should have fired"
        );
    }
}
