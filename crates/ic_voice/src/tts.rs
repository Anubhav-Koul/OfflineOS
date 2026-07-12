//! Text-to-speech via the Piper subprocess.
//!
//! Piper (the archived MIT `rhasspy/piper` build — *not* the GPL `piper1-gpl`
//! Python package) is a self-contained binary. We drive it one utterance at a time:
//! spawn `piper --model <voice.onnx> --output-raw`, write the line of text to its
//! stdin, close stdin, and read raw 16-bit little-endian mono PCM from its stdout
//! until it exits. One-shot per utterance means EOF *is* the end of the audio — no
//! framing to invent over a persistent stream — and nothing is ever written to
//! disk.
//!
//! Each spawned child is enlisted in the widget's kill-on-close Job Object through
//! the [`ChildEnlist`] hook (the same guarantee `llama-server` and the browser
//! sidecar get), so a hard kill of the widget takes any in-flight synthesis down
//! with it. Piper emits no timing metadata, so lip sync is driven downstream from
//! the RMS of this PCM ([`crate::envelope`]) rather than from anything Piper says.
//!
//! The subprocess round-trip is synchronous (`std::process`) and blocking; the
//! driver runs [`PiperTts::synthesize`] on a blocking thread. The pure parts —
//! decoding the PCM ([`crate::format::pcm_i16le_to_f32`]) and reading the voice's
//! sample rate from its config — are unit-tested; the spawn path is covered by an
//! `#[ignore]`d test that needs a real `piper.exe`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::format;
use crate::stages::{Speech, Synthesizer};

/// Enlists a freshly-spawned child in the parent's kill-on-close job. Mirrors
/// `ic_llama::SpawnHook`, but for the synchronous `std::process::Child` Piper uses
/// (the widget builds this from its `ProcessJob`). Returning `Err` fails the spawn
/// and the child is killed, so a TTS process that cannot be contained never runs.
pub type ChildEnlist = Arc<dyn Fn(&std::process::Child) -> std::io::Result<()> + Send + Sync>;

/// An enlist hook that does nothing — for a standalone Piper with no job to join
/// (tests, and any path where the parent is not the widget).
pub fn no_enlist() -> ChildEnlist {
    Arc::new(|_| Ok(()))
}

/// Piper's default output rate when a voice config does not state one.
const DEFAULT_SAMPLE_RATE: u32 = 22_050;

/// A Piper voice: the executable, the `.onnx` model, and the rate it renders at.
pub struct PiperTts {
    exe: PathBuf,
    voice_model: PathBuf,
    sample_rate: u32,
    enlist: ChildEnlist,
}

impl PiperTts {
    /// Build a synthesizer for `voice_model` driven by the Piper `exe`. The output
    /// sample rate is read from the voice's config JSON (`<voice_model>.json`),
    /// falling back to Piper's default if the config is absent or unparseable.
    ///
    /// Enlist each spawned child via `enlist`; pass [`no_enlist`] for a standalone
    /// Piper with no Job Object to join.
    pub fn new(
        exe: impl Into<PathBuf>,
        voice_model: impl Into<PathBuf>,
        enlist: ChildEnlist,
    ) -> Self {
        let exe = exe.into();
        let voice_model = voice_model.into();
        let sample_rate = read_voice_sample_rate(&voice_model).unwrap_or_else(|| {
            tracing::warn!(
                voice = %voice_model.display(),
                default = DEFAULT_SAMPLE_RATE,
                "no readable Piper voice config; assuming the default sample rate"
            );
            DEFAULT_SAMPLE_RATE
        });
        Self {
            exe,
            voice_model,
            sample_rate,
            enlist,
        }
    }

    /// The rate this voice renders at (from its config, or the default).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Spawn Piper for one line of text and collect its PCM. Blocking.
    fn run(&self, text: &str) -> Result<Vec<f32>> {
        let mut child = Command::new(&self.exe)
            .arg("--model")
            .arg(&self.voice_model)
            .arg("--output-raw")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| {
                Error::io(format!("spawning Piper at {}", self.exe.display()), source)
            })?;

        // Enlist before we do anything else — a Piper that hangs must still die
        // with the widget. A hook failure kills the child rather than orphaning it.
        if let Err(source) = (self.enlist)(&child) {
            let _ = child.kill();
            return Err(Error::io("enlisting Piper in the process job", source));
        }

        // Piper reads ONE line per utterance; the newline commits it. The text must
        // therefore be a single line — a multi-line reply would (a) be synthesized
        // as N separate utterances and, worse, (b) deadlock the pipes: Piper starts
        // writing line 1's PCM to a stdout nobody drains yet while we are still
        // blocked writing the remaining lines into its full stdin. A single line is
        // safe at any length, because Piper consumes stdin up to the newline before
        // it produces any output. Dropping stdin then signals end-of-input so the
        // process finishes and exits.
        {
            let line = single_line(text);
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| Error::Tts("Piper stdin was not piped".into()))?;
            stdin
                .write_all(line.as_bytes())
                .and_then(|()| stdin.write_all(b"\n"))
                .map_err(|source| Error::io("writing text to Piper", source))?;
        }

        // `wait_with_output` drains stdout and stderr concurrently on its own
        // threads, so a chatty stderr can't deadlock a large stdout.
        let output = child
            .wait_with_output()
            .map_err(|source| Error::io("reading Piper output", source))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Tts(format!(
                "Piper exited with {}: {}",
                output.status,
                stderr.trim()
            )));
        }

        Ok(format::pcm_i16le_to_f32(&output.stdout))
    }
}

impl Synthesizer for PiperTts {
    fn synthesize(&mut self, text: &str) -> Result<Speech> {
        if text.trim().is_empty() {
            return Ok(Speech {
                samples: Vec::new(),
                sample_rate: self.sample_rate,
            });
        }
        let samples = self.run(text)?;
        Ok(Speech {
            samples,
            sample_rate: self.sample_rate,
        })
    }
}

/// Collapse all whitespace runs (including newlines) to single spaces, yielding the
/// one line Piper expects per utterance. See the pipe-deadlock note in
/// [`PiperTts::run`].
fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Read the `audio.sample_rate` from a Piper voice config that sits beside the
/// model as `<voice_model>.json`. `None` if the file is missing or has no rate.
fn read_voice_sample_rate(voice_model: &Path) -> Option<u32> {
    // Piper's convention: config is the model path with `.json` appended, e.g.
    // `en_US-amy-medium.onnx` -> `en_US-amy-medium.onnx.json`.
    let mut config = voice_model.as_os_str().to_owned();
    config.push(".json");
    let text = std::fs::read_to_string(PathBuf::from(config)).ok()?;
    parse_sample_rate(&text)
}

/// Extract `audio.sample_rate` from a Piper voice config JSON body.
///
/// Kept as a pure string function (rather than pulling in a JSON dependency for one
/// integer) and unit-tested. Tolerates whitespace and the field appearing anywhere.
fn parse_sample_rate(config_json: &str) -> Option<u32> {
    // Find `"sample_rate"`, then the first integer after the following colon.
    let key = config_json.find("\"sample_rate\"")?;
    let after_colon = config_json[key..].find(':')? + key + 1;
    let digits: String = config_json[after_colon..]
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sample_rate_is_read_from_a_piper_config_body() {
        let config = r#"{ "audio": { "sample_rate": 22050 }, "espeak": {} }"#;
        assert_eq!(parse_sample_rate(config), Some(22_050));
    }

    #[test]
    fn a_config_without_a_rate_yields_none() {
        assert_eq!(parse_sample_rate(r#"{ "audio": {} }"#), None);
        assert_eq!(parse_sample_rate("not json at all"), None);
    }

    #[test]
    fn whitespace_around_the_value_is_tolerated() {
        assert_eq!(
            parse_sample_rate("\"sample_rate\"   :\n   16000,"),
            Some(16_000)
        );
    }

    #[test]
    fn multi_line_text_collapses_to_one_line_for_piper() {
        // A multi-line LLM reply must become a single utterance line — newlines
        // into Piper's stdin deadlock the pipes on long replies.
        let reply = "First paragraph.\n\nSecond one,\r\nwith a wrapped   line.";
        assert_eq!(
            single_line(reply),
            "First paragraph. Second one, with a wrapped line."
        );
        assert!(!single_line(reply).contains('\n'));
    }

    #[test]
    fn empty_text_synthesizes_to_silence_without_spawning() {
        // exe path is bogus; empty text must short-circuit before any spawn.
        let mut tts = PiperTts::new("nonexistent-piper.exe", "voice.onnx", no_enlist());
        let speech = tts.synthesize("   ").expect("empty text is not an error");
        assert!(speech.is_empty());
        assert_eq!(speech.sample_rate, DEFAULT_SAMPLE_RATE);
    }

    /// Real synthesis needs a bundled `piper.exe` and a voice model; wired by the
    /// widget at runtime. Run with `--ignored` once assets are present.
    #[test]
    #[ignore = "needs a real piper.exe + voice model; run with --ignored"]
    fn piper_synthesizes_audible_pcm() {
        let exe = std::env::var("IC_VOICE_PIPER_EXE").expect("set IC_VOICE_PIPER_EXE");
        let voice = std::env::var("IC_VOICE_PIPER_VOICE").expect("set IC_VOICE_PIPER_VOICE");
        let mut tts = PiperTts::new(exe, voice, no_enlist());
        let speech = tts.synthesize("Hello from IronClaw.").expect("synthesis");
        assert!(!speech.is_empty(), "expected audio");
        assert!(speech.sample_rate >= 16_000);
        // A real utterance is a good fraction of a second.
        assert!(speech.duration_secs() > 0.3, "{}", speech.duration_secs());
    }
}
