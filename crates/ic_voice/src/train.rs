//! Training a wake word from the user's own voice.
//!
//! rustpotter is a **reference-model** spotter, not a neural one: it has no
//! pretrained vocabulary and no built-in phrase. A wake word *is* a handful of
//! recordings of someone saying it, reduced to feature vectors. That is why the app
//! could not ship one — and it is also why it can make one in thirty seconds, on
//! the user's machine, from the user's own voice. Nothing is uploaded.
//!
//! The wake word is the assistant's **name**, which is why this belongs to the
//! setup wizard: the same step that names the character teaches the machine to hear
//! it.
//!
//! The output is a `.rpw` file in the widget's wake-model directory, which is
//! exactly where [`crate::bundled_wake_models`] already looks — so training one
//! flips the pipeline from [`crate::NullWakeWord`] (push-to-talk only) to
//! [`crate::RustpotterWake`] on the next start, with no other wiring.

use std::path::{Path, PathBuf};

use std::collections::HashMap;

use rustpotter::Wakeword;

use crate::error::{Error, Result};
use crate::format::{SAMPLE_RATE, f32_to_i16};

/// Fewer than this and the model overfits one utterance: the user says their
/// assistant's name slightly differently every time, and an average of three is the
/// smallest thing that generalises across those.
pub const MIN_SAMPLES: usize = 3;

/// A single recorded utterance of the wake phrase, as 16 kHz mono f32.
pub type Sample = Vec<f32>;

/// Encode PCM as a 16-bit mono WAV. rustpotter reads WAV, not raw samples, so the
/// header is not optional.
fn wav_bytes(samples: &[f32]) -> Vec<u8> {
    let pcm = f32_to_i16(samples);
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + pcm.len() * 2);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in pcm {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// The file a trained wake word is written to.
pub fn model_path(wake_dir: &Path, name: &str) -> PathBuf {
    // The name is the assistant's, chosen by the user, so it cannot be trusted as a
    // filename. Reduce it to a safe slug; the *model's* own name keeps the original.
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    let slug = if slug.is_empty() {
        "wakeword".to_string()
    } else {
        slug
    };
    wake_dir.join(format!("{slug}.rpw"))
}

/// Train a wake word called `name` from `samples` and save it under `wake_dir`.
///
/// Returns the path written. The pipeline picks it up on its next start.
pub fn train(wake_dir: &Path, name: &str, samples: &[Sample]) -> Result<PathBuf> {
    if samples.len() < MIN_SAMPLES {
        return Err(Error::model(format!(
            "a wake word needs at least {MIN_SAMPLES} recordings; got {}",
            samples.len()
        )));
    }
    // A silent "recording" trains a model that fires on silence — which means the
    // agent wakes constantly and the user cannot work out why. Refuse it here rather
    // than shipping a model that is worse than none.
    for (index, sample) in samples.iter().enumerate() {
        if peak(sample) < 0.02 {
            return Err(Error::model(format!(
                "recording {} is silent — check the microphone and try again",
                index + 1
            )));
        }
    }

    let mut buffers: HashMap<String, Vec<u8>> = HashMap::new();
    for (index, sample) in samples.iter().enumerate() {
        buffers.insert(format!("{name}-{index}.wav"), wav_bytes(sample));
    }

    // Thresholds left to rustpotter's defaults: they are computed against the
    // averaged features of these very recordings, so a hand-picked number here would
    // be a guess about a distribution we have not seen.
    let wakeword = Wakeword::new_from_sample_buffers(name.to_string(), None, None, buffers)
        .map_err(|reason| Error::model(format!("could not train the wake word: {reason}")))?;

    std::fs::create_dir_all(wake_dir)
        .map_err(|source| Error::io(format!("creating {}", wake_dir.display()), source))?;
    let path = model_path(wake_dir, name);
    wakeword
        .save_to_file(&path.to_string_lossy())
        .map_err(|reason| Error::model(format!("could not save the wake word: {reason}")))?;

    tracing::info!(name, path = %path.display(), samples = samples.len(), "trained a wake word");
    Ok(path)
}

/// Loudest sample, for the silence check and the UI's level meter.
pub fn peak(samples: &[f32]) -> f32 {
    samples
        .iter()
        .fold(0.0_f32, |loudest, sample| loudest.max(sample.abs()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recognisable tone, so training has something with structure to chew on.
    fn tone(seconds: f32) -> Sample {
        let count = (SAMPLE_RATE as f32 * seconds) as usize;
        (0..count)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                0.4 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
            })
            .collect()
    }

    #[test]
    fn the_wav_header_says_16_bit_mono_at_the_pipeline_rate() {
        let wav = wav_bytes(&tone(0.1));
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1, "mono");
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            SAMPLE_RATE
        );
        assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16, "bits");
    }

    /// The whole point: a trained model lands where the pipeline already looks, so
    /// the next start swaps `NullWakeWord` for `RustpotterWake` with no other wiring.
    #[test]
    fn training_writes_a_model_the_pipeline_will_find() {
        let dir = tempfile::tempdir().expect("tempdir");
        let samples = vec![tone(0.9), tone(1.0), tone(0.95)];

        let path = train(dir.path(), "Nova", &samples).expect("trains");

        assert_eq!(path, dir.path().join("nova.rpw"));
        assert!(path.is_file());
        assert_eq!(crate::bundled_wake_models(dir.path()), vec![path]);
    }

    #[test]
    fn too_few_recordings_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = train(dir.path(), "Nova", &[tone(1.0)]).expect_err("must refuse");
        assert!(error.to_string().contains("at least 3"), "{error}");
    }

    /// A model trained on silence fires on silence: the character would wake
    /// constantly and the user would never work out why. Better to refuse.
    #[test]
    fn a_silent_recording_is_refused_rather_than_trained_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let samples = vec![tone(1.0), vec![0.0; SAMPLE_RATE as usize], tone(1.0)];
        let error = train(dir.path(), "Nova", &samples).expect_err("must refuse");
        assert!(error.to_string().contains("silent"), "{error}");
    }

    /// The assistant's name is user input, and it becomes a filename.
    #[test]
    fn a_hostile_name_cannot_escape_the_wake_directory() {
        let path = model_path(Path::new("/wake"), "../../etc/passwd");
        assert_eq!(path.parent(), Some(Path::new("/wake")));
        assert!(!path.to_string_lossy().contains(".."));
    }
}
