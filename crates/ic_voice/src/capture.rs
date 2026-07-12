//! Microphone capture: cpal → downmix → resample → the shared ring.
//!
//! Opens the default WASAPI input device, and on every callback converts the
//! device's format (any channel count, `f32` or `i16`, any rate) into 16 kHz mono
//! `f32` and writes it to a [`SampleRing`] the wake-word / VAD / whisper stages
//! read from. The callback does the minimum and never blocks — the ring absorbs
//! bursts and the readers run elsewhere.
//!
//! Capture sits behind the [`Capture`] trait so the pipeline can be driven by a
//! fake that plays canned audio into the ring, with no microphone. The cpal
//! implementation is only exercised by the `#[ignore]`d real-device test.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::error::{Error, Result};
use crate::format::{self, SAMPLE_RATE};
use crate::resample::Resampler;
use crate::ring::SampleRing;

/// A running microphone capture, writing 16 kHz mono into a ring.
pub trait Capture: Send {
    /// The ring this capture writes into.
    fn ring(&self) -> SampleRing;
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
    /// Open the default input device and start capturing into a fresh ring sized
    /// for `ring_seconds` of audio.
    pub fn start(ring_seconds: f32) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| Error::NoDevice("no default input device".into()))?;
        let config = device
            .default_input_config()
            .map_err(|error| Error::audio(format!("reading the input config: {error}")))?;

        let ring = SampleRing::for_seconds(ring_seconds);
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

        tracing::info!(
            input_rate,
            channels,
            ?sample_format,
            target_rate = SAMPLE_RATE,
            "microphone capture started"
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
