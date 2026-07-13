//! Microphone capture: cpal → downmix → resample → the shared ring.
//!
//! Opens a WASAPI input device, and on every callback converts the device's format
//! (any channel count, `f32` or `i16`, any rate) into 16 kHz mono `f32` and writes
//! it to a [`SampleRing`] the wake-word / VAD / whisper stages read from. The
//! callback does the minimum and never blocks — the ring absorbs bursts and the
//! readers run elsewhere.
//!
//! **The default device cannot be trusted.** A paired Bluetooth speaker or soundbar
//! registers a "Headset" (HFP) capture endpoint that Windows happily makes the
//! default — and that endpoint opens cleanly, reports healthy, and then delivers
//! **zero samples forever** unless the headset engages call mode (observed on real
//! hardware: the default endpoint produced nothing while the actual microphone one
//! device down worked perfectly). So [`CpalCapture::start`] *verifies audio
//! actually flows* within a short probe window and falls back through the other
//! input devices; only a machine where no device delivers is an error. Note that a
//! live-but-quiet room still delivers callbacks (silence is samples), so "no
//! callbacks" reliably means a dead endpoint, not a quiet one.
//!
//! Capture sits behind the [`Capture`] trait so the pipeline can be driven by a
//! fake that plays canned audio into the ring, with no microphone. The cpal
//! implementation is only exercised by the `#[ignore]`d real-device test.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::error::{Error, Result};
use crate::format::{self, SAMPLE_RATE};
use crate::resample::Resampler;
use crate::ring::SampleRing;

/// How long a freshly-opened device gets to deliver its first samples before it is
/// declared dead and the next device is tried. A live mic's first callback arrives
/// within tens of milliseconds (plus one resampler chunk of buffering); half a
/// second is generous.
const PROBE_WINDOW: Duration = Duration::from_millis(500);

/// A running microphone capture, writing 16 kHz mono into a ring.
pub trait Capture: Send {
    /// The ring this capture writes into.
    fn ring(&self) -> SampleRing;
    /// Whether the device is still delivering audio. `false` after a stream error
    /// (device unplugged without a default-change notification); the driver drops
    /// the capture and reopens. Defaults to healthy for fakes.
    fn is_healthy(&self) -> bool {
        true
    }
    /// Stop capturing and release the device.
    fn stop(self: Box<Self>);
}

/// cpal-backed capture from the default input device.
pub struct CpalCapture {
    stream: cpal::Stream,
    ring: SampleRing,
    // Kept so `is_capturing` reflects a stream that errored out from under us.
    healthy: Arc<AtomicBool>,
}

impl CpalCapture {
    /// Start capturing into a fresh ring sized for `ring_seconds` of audio, from
    /// the first input device that **actually delivers samples** — the default
    /// device first, then every other input. A device that opens but produces
    /// nothing within [`PROBE_WINDOW`] (the dead Bluetooth-headset-endpoint
    /// failure mode) is skipped, not trusted.
    ///
    /// Blocks up to `PROBE_WINDOW` per dead device; the driver calls this off any
    /// latency-sensitive path (startup, unmute, device change).
    pub fn start(ring_seconds: f32) -> Result<Self> {
        Self::start_on(None, ring_seconds)
    }

    /// Start capture, preferring the input device named `preferred`.
    ///
    /// **The OS default is not good enough on its own.** A paired Bluetooth headset
    /// makes itself the default input, and its HFP endpoint opens cleanly, reports
    /// no error, and delivers a steady stream of *near-silence* — so it passes
    /// [`Self::delivers_audio`], which can only tell "produces samples" from
    /// "produces nothing". It cannot tell "produces samples that contain a voice",
    /// because a live mic in a quiet room looks the same. Measured on this machine:
    /// the default (`Headset (OnePlus Buds 4)`) peaked at 0.003 while the user's
    /// actual microphone sat unused. So the device must be a *choice*, and this is
    /// how the user makes it.
    ///
    /// A `preferred` device that is missing (unplugged since it was chosen) falls
    /// back to the automatic order rather than failing: no microphone at all is
    /// worse than the wrong one.
    pub fn start_on(preferred: Option<&str>, ring_seconds: f32) -> Result<Self> {
        let host = cpal::default_host();
        let ring = SampleRing::for_seconds(ring_seconds);

        // The user's choice first, then the OS default, then everything else —
        // deduplicated by description (the default also appears in the enumeration).
        let mut candidates: Vec<cpal::Device> = Vec::new();
        if let Some(preferred) = preferred {
            match host.input_devices() {
                Ok(devices) => candidates.extend(devices.filter(|device| {
                    device
                        .description()
                        .map(|d| d.to_string())
                        .is_ok_and(|name| name == preferred)
                })),
                Err(error) => tracing::warn!(%error, "could not enumerate input devices"),
            }
            if candidates.is_empty() {
                tracing::warn!(
                    device = preferred,
                    "the chosen microphone is not present; falling back"
                );
            }
        }
        candidates.extend(host.default_input_device());
        match host.input_devices() {
            Ok(devices) => candidates.extend(devices),
            Err(error) => tracing::warn!(%error, "could not enumerate input devices"),
        }

        let mut tried: Vec<String> = Vec::new();
        for device in candidates {
            let label = device
                .description()
                .map(|d| d.to_string())
                .unwrap_or_else(|_| "<unnamed input>".into());
            if tried.contains(&label) {
                continue;
            }
            tried.push(label.clone());

            let capture = match Self::open(&device, ring.clone()) {
                Ok(capture) => capture,
                Err(error) => {
                    tracing::warn!(device = %label, %error, "could not open an input device");
                    continue;
                }
            };
            if capture.delivers_audio(PROBE_WINDOW) {
                tracing::info!(device = %label, "microphone capture started");
                return Ok(capture);
            }
            // Opened cleanly, reported no error, delivered nothing: a dead
            // endpoint (e.g. a Bluetooth speaker's idle HFP mic). Next.
            tracing::warn!(
                device = %label,
                "input device delivered no audio; trying the next one"
            );
            drop(capture);
        }

        Err(Error::NoDevice(if tried.is_empty() {
            "no input device present".into()
        } else {
            format!(
                "no input device delivered audio (tried: {})",
                tried.join(", ")
            )
        }))
    }

    /// Wait up to `window` for the first samples to land in the ring. A live mic
    /// delivers callbacks even in a silent room, so an empty ring after the window
    /// means the endpoint is dead.
    fn delivers_audio(&self, window: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < window {
            if !self.ring.is_empty() {
                return true;
            }
            if !self.is_healthy() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        false
    }

    /// Open a capture stream on `device`, writing 16 kHz mono into `ring`.
    fn open(device: &cpal::Device, ring: SampleRing) -> Result<Self> {
        let config = device
            .default_input_config()
            .map_err(|error| Error::audio(format!("reading the input config: {error}")))?;

        let channels = config.channels() as usize;
        // cpal 0.18's `SampleRate` is a `u32` alias, not a newtype.
        let input_rate = config.sample_rate();
        let sample_format = config.sample_format();
        let stream_config: cpal::StreamConfig = config.into();

        let healthy = Arc::new(AtomicBool::new(true));
        let err_healthy = Arc::clone(&healthy);
        let on_error = move |error| {
            tracing::warn!(%error, "the capture stream errored");
            err_healthy.store(false, Ordering::Relaxed);
        };

        // One resampler per stream, owned by the callback. Mono downmix happens
        // first (cheap), then resample to 16 kHz, then write.
        let mut resampler = Resampler::new(input_rate)?;
        let write_ring = ring.clone();

        let stream = match sample_format {
            cpal::SampleFormat::F32 => device.build_input_stream(
                stream_config,
                move |data: &[f32], _: &_| {
                    let mono = format::downmix_to_mono(data, channels);
                    write_ring.write(&resampler.push(&mono));
                },
                on_error,
                None,
            ),
            cpal::SampleFormat::I16 => device.build_input_stream(
                stream_config,
                move |data: &[i16], _: &_| {
                    let floats = format::i16_to_f32(data);
                    let mono = format::downmix_to_mono(&floats, channels);
                    write_ring.write(&resampler.push(&mono));
                },
                on_error,
                None,
            ),
            other => {
                return Err(Error::audio(format!(
                    "unsupported capture sample format: {other:?}"
                )));
            }
        }
        .map_err(|error| Error::audio(format!("building the capture stream: {error}")))?;

        stream
            .play()
            .map_err(|error| Error::audio(format!("starting the capture stream: {error}")))?;

        tracing::debug!(
            input_rate,
            channels,
            ?sample_format,
            target_rate = SAMPLE_RATE,
            "capture stream opened; probing for audio"
        );
        Ok(Self {
            stream,
            ring,
            healthy,
        })
    }

    /// Whether the stream is still delivering audio (false after a device error).
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }
}

impl Capture for CpalCapture {
    fn ring(&self) -> SampleRing {
        self.ring.clone()
    }

    fn is_healthy(&self) -> bool {
        CpalCapture::is_healthy(self)
    }

    fn stop(self: Box<Self>) {
        // Dropping the stream stops capture and frees the device; pausing first is
        // the graceful path.
        let _ = self.stream.pause();
        drop(self.stream);
    }
}

/// A fake capture that plays pre-recorded samples into the ring, for driving the
/// pipeline with no microphone. `feed` pushes audio as if the mic had produced it.
#[cfg(any(test, feature = "test-support"))]
pub struct FakeCapture {
    ring: SampleRing,
}

#[cfg(any(test, feature = "test-support"))]
impl FakeCapture {
    /// A fake writing into a ring sized for `ring_seconds`.
    pub fn new(ring_seconds: f32) -> Self {
        Self {
            ring: SampleRing::for_seconds(ring_seconds),
        }
    }

    /// A fake writing into an existing (shared) ring, so a test can feed audio
    /// through a ring it also holds — the way the driver's capture factory hands
    /// out captures that share the test's feed handle.
    pub fn with_ring(ring: SampleRing) -> Self {
        Self { ring }
    }

    /// Push samples as if captured from the mic (already 16 kHz mono).
    pub fn feed(&self, samples: &[f32]) {
        self.ring.write(samples);
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Capture for FakeCapture {
    fn ring(&self) -> SampleRing {
        self.ring.clone()
    }
    fn stop(self: Box<Self>) {}
}

/// Every input device the machine offers, with the OS default first.
///
/// The UI needs this because the default is often wrong: a paired Bluetooth
/// headset takes the default input slot and then hears nothing (see
/// [`CpalCapture::start_on`]). Names are what [`CpalCapture::start_on`] matches on.
pub fn input_devices() -> Vec<String> {
    let host = cpal::default_host();
    let mut names: Vec<String> = Vec::new();

    let describe = |device: &cpal::Device| device.description().map(|d| d.to_string()).ok();

    if let Some(name) = host.default_input_device().as_ref().and_then(describe) {
        names.push(name);
    }
    if let Ok(devices) = host.input_devices() {
        for device in devices {
            if let Some(name) = describe(&device)
                && !names.contains(&name)
            {
                names.push(name);
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fake_capture_delivers_fed_audio_through_the_ring() {
        let capture = FakeCapture::new(1.0);
        let ring = capture.ring();
        capture.feed(&[0.1, 0.2, 0.3]);
        assert_eq!(ring.latest(3), [0.1, 0.2, 0.3]);
    }

    /// Opens the real default microphone. Ignored: CI has no audio device.
    #[test]
    #[ignore = "needs a real microphone; run with --ignored"]
    fn cpal_capture_opens_the_default_mic_and_produces_16k_audio() {
        let capture = match CpalCapture::start(2.0) {
            Ok(capture) => capture,
            Err(error) => {
                eprintln!("skipping: no usable mic ({error})");
                return;
            }
        };
        let ring = capture.ring();
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(capture.is_healthy(), "the stream should still be healthy");
        // Half a second of 16 kHz audio should have landed (~8000 samples), give
        // or take startup latency.
        assert!(
            ring.len() > 1000,
            "expected captured audio, got {} samples",
            ring.len()
        );
        Box::new(capture).stop();
    }
}
