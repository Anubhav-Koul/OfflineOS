//! The real-model round trip: Piper speaks a sentence, the crate's own resampler
//! brings it to 16 kHz, and whisper transcribes it back. This exercises the exact
//! audio path the app uses (TTS PCM → `Resampler` → STT) with the genuine binaries
//! and models, so it catches what the unit fakes cannot: a wrong Piper CLI flag, a
//! sample-rate mismatch, PCM byte-order confusion, or a whisper build that loads
//! but mis-hears.
//!
//! Ignored: needs the pinned assets on disk. Run with:
//!
//! ```text
//! IC_VOICE_PIPER_EXE=…\piper.exe \
//! IC_VOICE_PIPER_VOICE=…\en_US-amy-medium.onnx \
//! IC_VOICE_WHISPER_MODEL=…\ggml-base.en.bin \
//!   cargo test -p ic_voice --test real_voice_loop -- --ignored --nocapture
//! ```

use ic_voice::stages::{Synthesizer, Transcriber};
use ic_voice::{PiperTts, Resampler, SAMPLE_RATE, WhisperStt, no_enlist};

#[test]
#[ignore = "needs real piper + whisper assets; run with --ignored"]
fn piper_speaks_and_whisper_hears_the_same_sentence() {
    let piper_exe = std::env::var("IC_VOICE_PIPER_EXE").expect("set IC_VOICE_PIPER_EXE");
    let piper_voice = std::env::var("IC_VOICE_PIPER_VOICE").expect("set IC_VOICE_PIPER_VOICE");
    let whisper_model =
        std::env::var("IC_VOICE_WHISPER_MODEL").expect("set IC_VOICE_WHISPER_MODEL");

    // 1. Speak.
    let mut tts = PiperTts::new(piper_exe, piper_voice, no_enlist());
    let sentence = "The quick brown fox jumps over the lazy dog.";
    let speech = tts.synthesize(sentence).expect("synthesis");
    assert!(
        speech.duration_secs() > 1.0,
        "a full sentence should be over a second, got {}s",
        speech.duration_secs()
    );

    // 2. Resample to the pipeline rate, exactly as playback/STT would.
    let mut resampler =
        Resampler::to_rate(speech.sample_rate, SAMPLE_RATE).expect("build resampler");
    let mut audio = resampler.push(&speech.samples);
    audio.extend(resampler.flush());
    assert!(
        audio.len() > SAMPLE_RATE as usize,
        "expected >1s of 16 kHz audio, got {} samples",
        audio.len()
    );

    // 3. Hear.
    let mut stt = WhisperStt::new(whisper_model).expect("load whisper");
    let transcript = stt.transcribe(&audio).expect("transcription");
    eprintln!("transcript: {transcript:?}");

    let lower = transcript.to_lowercase();
    for word in ["quick", "brown", "fox", "lazy", "dog"] {
        assert!(
            lower.contains(word),
            "transcript {transcript:?} lost the word {word:?}"
        );
    }
}
