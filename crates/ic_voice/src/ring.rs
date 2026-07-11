//! A bounded ring buffer of mono samples, shared across threads.
//!
//! Capture runs on the audio callback thread and *writes*; the wake-word / VAD /
//! whisper stages run elsewhere and *read*. A bounded ring decouples them: the
//! callback never blocks (the cardinal rule of an audio callback — blocking it
//! drops samples system-wide), and a slow reader loses the oldest audio rather
//! than growing memory without bound.
//!
//! Losing old audio is the right failure: for a wake word or a live utterance,
//! stale samples are worthless. The buffer holds a few seconds, sized by the
//! caller in samples at [`crate::format::SAMPLE_RATE`].

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// A thread-safe bounded ring of `f32` samples.
#[derive(Clone)]
pub struct SampleRing {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    buffer: VecDeque<f32>,
    capacity: usize,
    /// How many samples were dropped because the reader fell behind — surfaced so
    /// a wedged reader is observable rather than silent.
    dropped: u64,
}

impl SampleRing {
    /// A ring holding at most `capacity` samples.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                buffer: VecDeque::with_capacity(capacity),
                capacity: capacity.max(1),
                dropped: 0,
            })),
        }
    }

    /// A ring sized to hold `seconds` of audio at the pipeline sample rate.
    pub fn for_seconds(seconds: f32) -> Self {
        Self::with_capacity((crate::format::SAMPLE_RATE as f32 * seconds) as usize)
    }

    /// Append samples. Never blocks and never grows past capacity: once full, each
    /// new sample evicts the oldest. Safe to call from the audio callback.
    pub fn write(&self, samples: &[f32]) {
        let Ok(mut inner) = self.inner.lock() else {
            return; // a poisoned lock must not panic the audio thread
        };
        for &sample in samples {
            if inner.buffer.len() == inner.capacity {
                inner.buffer.pop_front();
                inner.dropped += 1;
            }
            inner.buffer.push_back(sample);
        }
    }

    /// Copy out the most recent `count` samples (fewer if the ring holds fewer),
    /// oldest first, without consuming them. For a wake-word window that slides
    /// over the same audio each tick.
    pub fn latest(&self, count: usize) -> Vec<f32> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        let available = inner.buffer.len();
        let start = available.saturating_sub(count);
        inner.buffer.iter().skip(start).copied().collect()
    }

    /// Drain everything currently buffered, oldest first, emptying the ring. For
    /// consuming a captured utterance once VAD says it ended.
    pub fn drain(&self) -> Vec<f32> {
        let Ok(mut inner) = self.inner.lock() else {
            return Vec::new();
        };
        inner.buffer.drain(..).collect()
    }

    /// Number of samples currently buffered.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .map(|inner| inner.buffer.len())
            .unwrap_or(0)
    }

    /// Whether the ring is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total samples ever dropped because the reader fell behind. A steadily
    /// climbing value means the pipeline can't keep up with capture.
    pub fn dropped(&self) -> u64 {
        self.inner.lock().map(|inner| inner.dropped).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_below_capacity_are_all_retained_in_order() {
        let ring = SampleRing::with_capacity(8);
        ring.write(&[1.0, 2.0, 3.0]);
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.drain(), [1.0, 2.0, 3.0]);
        assert!(ring.is_empty());
    }

    #[test]
    fn overflow_evicts_the_oldest_and_counts_the_drop() {
        let ring = SampleRing::with_capacity(3);
        ring.write(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        // Only the last 3 survive; 2 were dropped.
        assert_eq!(ring.drain(), [3.0, 4.0, 5.0]);
        assert_eq!(ring.dropped(), 2);
    }

    #[test]
    fn latest_returns_the_most_recent_window_without_consuming() {
        let ring = SampleRing::with_capacity(10);
        ring.write(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(ring.latest(2), [3.0, 4.0]);
        // Non-destructive: the samples are still there.
        assert_eq!(ring.len(), 4);
        // Asking for more than held returns everything.
        assert_eq!(ring.latest(99), [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn the_ring_is_shareable_across_clones() {
        let writer = SampleRing::with_capacity(4);
        let reader = writer.clone();
        writer.write(&[9.0, 8.0]);
        // The clone sees the same buffer.
        assert_eq!(reader.latest(2), [9.0, 8.0]);
    }

    #[test]
    fn for_seconds_sizes_by_the_pipeline_rate() {
        let ring = SampleRing::for_seconds(2.0);
        ring.write(&vec![0.0; crate::format::SAMPLE_RATE as usize * 3]);
        // Holds exactly 2 seconds; the extra second was evicted.
        assert_eq!(ring.len(), crate::format::SAMPLE_RATE as usize * 2);
    }
}
