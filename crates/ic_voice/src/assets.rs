//! Provisioning the voice models: whisper, the Piper voice, and the Piper binary.
//!
//! Voice needs assets that are too big to commit: the whisper STT model, a Piper
//! voice (`.onnx` + its `.json`), and the Piper executable (with its DLLs). Each is
//! **pinned by URL, SHA-256, and size** — the same discipline as
//! `ic_llama::release` — and fetched through `ic_llama`'s resumable, digest-verified
//! [`Downloader`], so a half-download resumes and a corrupted file is re-fetched
//! rather than trusted. Nothing is verified by name alone.
//!
//! The wake-word model is *not* here: we ship our own rustpotter reference models
//! in-tree (recorded, so the licence is clean), located with [`bundled_wake_models`].
//!
//! Digests were taken from the sources on 2026-07-12:
//! - whisper `ggml-small.en.bin` — HuggingFace `ggerganov/whisper.cpp` (MIT).
//! - Piper voice `en_US-amy-medium` — HuggingFace `rhasspy/piper-voices`
//!   (**verify the voice's MODEL_CARD licence before public release** — Phase 6).
//! - Piper `2023.11.14-2` `piper_windows_amd64.zip` — `rhasspy/piper` (MIT).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ic_llama::Sha256Hex;
use ic_llama::download::{DownloadRequest, Downloader, Progress};

use crate::error::{Error, Result};

/// A pinned downloadable asset.
#[derive(Debug, Clone, Copy)]
pub struct PinnedAsset {
    /// A human label for progress/logging.
    pub label: &'static str,
    /// Where to fetch it.
    pub url: &'static str,
    /// Expected SHA-256 of the complete file (lowercase hex).
    pub sha256: &'static str,
    /// Expected size in bytes (for progress and a sanity log).
    pub size_bytes: u64,
    /// File name to store it under, within the voice asset directory.
    pub file_name: &'static str,
}

impl PinnedAsset {
    /// The validated digest. Errors only if the pinned constant is malformed, which
    /// a unit test rules out at `cargo test`.
    pub fn digest(&self) -> Result<Sha256Hex> {
        Sha256Hex::new(self.sha256)
            .map_err(|error| Error::model(format!("bad pinned digest for {}: {error}", self.label)))
    }
}

/// whisper `small.en` GGML (488 MB). English-only, CPU.
///
/// **Not `base.en` (147 MB), which was here first.** Measured on this hardware, over
/// a Bluetooth headset: `base.en` transcribed "Nova, can you check my emails?" as
/// "North and New check my emails." — the content words right, the short function
/// words at the front invented. Bluetooth HFP audio is compressed and narrowband,
/// and that is precisely where the small model runs out of context to disambiguate
/// with. Signal processing had already been pushed as far as it goes (silence
/// trimmed, level normalized against a click-proof estimate); what was left was the
/// model.
///
/// The cost is honest: 488 MB instead of 147, and slower on CPU. Worth it — a
/// wake word *is* a short function word, and an assistant that mishears its own name
/// is not an assistant.
pub const WHISPER_MODEL: PinnedAsset = PinnedAsset {
    label: "whisper small.en",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en.bin",
    sha256: "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d",
    size_bytes: 487_614_201,
    file_name: "ggml-small.en.bin",
};

/// One selectable Piper TTS voice (Phase 8c voice picker).
///
/// The model and its config are each a [`PinnedAsset`] — pinned by URL, SHA-256,
/// and size like every other asset. The `.onnx` digests were taken from
/// HuggingFace's authoritative git-LFS metadata (`lfs.oid`, which git-LFS defines
/// as the content SHA-256 — cross-checked against amy's long-standing pin); the
/// tiny `.onnx.json` configs are not LFS, so their digests were computed from the
/// fetched files. See `docs/desktop/voice-notes.md`.
#[derive(Debug, Clone, Copy)]
pub struct PiperVoice {
    /// Stable catalog id — also the Piper voice's canonical name (`en_US-amy-medium`).
    pub id: &'static str,
    /// A short name for the picker (e.g. "Amy").
    pub display_name: &'static str,
    /// Accent + timbre, for the picker's secondary line (e.g. "US · female").
    pub accent: &'static str,
    /// The voice model (`.onnx`).
    pub onnx: PinnedAsset,
    /// The voice config (`.onnx.json`, sits beside the model; Piper reads it).
    pub config: PinnedAsset,
}

/// The voice selected when the user has not chosen one — the voice that shipped
/// before the picker, so an existing install's sound does not change under it.
pub const DEFAULT_VOICE_ID: &str = "en_US-amy-medium";

/// The curated voice catalog.
///
/// **English only, deliberately.** A non-English Piper voice needs its language's
/// espeak-ng phoneme data, and our bundled Piper ships only what the English
/// voices use; listing a voice whose phonemes the bundle cannot resolve would
/// synthesize silence or gibberish. Every voice here shares the English espeak-ng
/// data amy already proves is present, and every one is `medium` quality at
/// 22.05 kHz — the sample rate flows from each config through the resampler
/// dynamically (`ic_voice::tts`/`playback`), so mixing rates would be safe too;
/// keeping them uniform just makes the picker predictable. Expanding to other
/// languages is gated on verifying the espeak-ng data ships for them.
pub const VOICES: &[PiperVoice] = &[
    PiperVoice {
        id: "en_US-amy-medium",
        display_name: "Amy",
        accent: "US · female",
        onnx: PinnedAsset {
            label: "Piper voice (Amy)",
            url: "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/amy/medium/en_US-amy-medium.onnx",
            sha256: "b3a6e47b57b8c7fbe6a0ce2518161a50f59a9cdd8a50835c02cb02bdd6206c18",
            size_bytes: 63_201_294,
            file_name: "en_US-amy-medium.onnx",
        },
        config: PinnedAsset {
            label: "Piper config (Amy)",
            url: "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/amy/medium/en_US-amy-medium.onnx.json",
            sha256: "95a23eb4d42909d38df73bb9ac7f45f597dbfcde2d1bf9526fdeaf5466977d77",
            size_bytes: 4_882,
            file_name: "en_US-amy-medium.onnx.json",
        },
    },
    PiperVoice {
        id: "en_US-lessac-medium",
        display_name: "Lessac",
        accent: "US · neutral",
        onnx: PinnedAsset {
            label: "Piper voice (Lessac)",
            url: "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/lessac/medium/en_US-lessac-medium.onnx",
            sha256: "5efe09e69902187827af646e1a6e9d269dee769f9877d17b16b1b46eeaaf019f",
            size_bytes: 63_201_294,
            file_name: "en_US-lessac-medium.onnx",
        },
        config: PinnedAsset {
            label: "Piper config (Lessac)",
            url: "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/lessac/medium/en_US-lessac-medium.onnx.json",
            sha256: "efe19c417bed055f2d69908248c6ba650fa135bc868b0e6abb3da181dab690a0",
            size_bytes: 4_885,
            file_name: "en_US-lessac-medium.onnx.json",
        },
    },
    PiperVoice {
        id: "en_US-ryan-medium",
        display_name: "Ryan",
        accent: "US · male",
        onnx: PinnedAsset {
            label: "Piper voice (Ryan)",
            url: "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/ryan/medium/en_US-ryan-medium.onnx",
            sha256: "abf4c274862564ed647ba0d2c47f8ee7c9b717d27bdad9219100eb310db4047a",
            size_bytes: 63_201_294,
            file_name: "en_US-ryan-medium.onnx",
        },
        config: PinnedAsset {
            label: "Piper config (Ryan)",
            url: "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/ryan/medium/en_US-ryan-medium.onnx.json",
            sha256: "44034c056cb15681b2ad494307c7f3f2e4499d1253c700c711fa0a4607ffe78d",
            size_bytes: 4_883,
            file_name: "en_US-ryan-medium.onnx.json",
        },
    },
    PiperVoice {
        id: "en_GB-alan-medium",
        display_name: "Alan",
        accent: "UK · male",
        onnx: PinnedAsset {
            label: "Piper voice (Alan)",
            url: "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_GB/alan/medium/en_GB-alan-medium.onnx",
            sha256: "0a309668932205e762801f1efc2736cd4b0120329622adf62be09e56339d3330",
            size_bytes: 63_201_294,
            file_name: "en_GB-alan-medium.onnx",
        },
        config: PinnedAsset {
            label: "Piper config (Alan)",
            url: "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_GB/alan/medium/en_GB-alan-medium.onnx.json",
            sha256: "c0f0d124e5895c00e7c03b35dcc8287f319a6998a365b182deb5c8e752ee8c1e",
            size_bytes: 4_888,
            file_name: "en_GB-alan-medium.onnx.json",
        },
    },
    PiperVoice {
        id: "en_GB-jenny_dioco-medium",
        display_name: "Jenny",
        accent: "UK · female",
        onnx: PinnedAsset {
            label: "Piper voice (Jenny)",
            url: "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_GB/jenny_dioco/medium/en_GB-jenny_dioco-medium.onnx",
            sha256: "469c630d209e139dd392a66bf4abde4ab86390a0269c1e47b4e5d7ce81526b01",
            size_bytes: 63_201_294,
            file_name: "en_GB-jenny_dioco-medium.onnx",
        },
        config: PinnedAsset {
            label: "Piper config (Jenny)",
            url: "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_GB/jenny_dioco/medium/en_GB-jenny_dioco-medium.onnx.json",
            sha256: "a9a7a93a317c9a3cb6563e37eb057df9ef09c06188a8a4341b0fcb58cba54dd4",
            size_bytes: 4_895,
            file_name: "en_GB-jenny_dioco-medium.onnx.json",
        },
    },
];

/// Find a voice by its catalog id.
pub fn find_voice(id: &str) -> Option<&'static PiperVoice> {
    VOICES.iter().find(|voice| voice.id == id)
}

/// Resolve an optional, possibly-stale voice id to a catalog voice, falling back
/// to the default when it is `None` or names a voice that no longer exists (a
/// dropped catalog entry from an older settings file must not disable voice).
pub fn voice_or_default(id: Option<&str>) -> &'static PiperVoice {
    id.and_then(find_voice)
        .or_else(|| find_voice(DEFAULT_VOICE_ID))
        .expect("the default voice is always in the catalog")
}

/// The Piper Windows binary bundle (piper.exe + espeak/onnx DLLs, 22 MB zip).
pub const PIPER_ARCHIVE: PinnedAsset = PinnedAsset {
    label: "Piper (Windows)",
    url: "https://github.com/rhasspy/piper/releases/download/2023.11.14-2/piper_windows_amd64.zip",
    sha256: "f3c58906402b24f3a96d92145f58acba6d86c9b5db896d207f78dc80811efcea",
    size_bytes: 22_477_236,
    file_name: "piper_windows_amd64.zip",
};

/// A callback invoked with `(asset label, progress)` as each asset downloads.
pub type AssetProgress = Arc<dyn Fn(&str, Progress) + Send + Sync>;

/// The resolved on-disk paths of the provisioned voice assets.
#[derive(Debug, Clone)]
pub struct VoiceAssets {
    /// The whisper GGML model.
    pub whisper_model: PathBuf,
    /// `piper.exe` (its sibling DLLs live beside it — do not move it).
    pub piper_exe: PathBuf,
    /// The Piper voice `.onnx` (its `.json` sits beside it).
    pub piper_voice: PathBuf,
}

impl VoiceAssets {
    /// The directory voice assets live under, within the app's model root.
    pub fn dir(root: &Path) -> PathBuf {
        root.join("voice")
    }

    /// The paths the assets *would* occupy for `voice`, without checking they
    /// exist. Whisper and `piper.exe` are voice-independent (one copy, shared);
    /// only `piper_voice` depends on which voice is selected.
    fn paths(root: &Path, voice: &PiperVoice) -> Self {
        let dir = Self::dir(root);
        Self {
            whisper_model: dir.join(WHISPER_MODEL.file_name),
            // The archive extracts to `piper/piper.exe` under the voice dir.
            piper_exe: dir.join("piper").join("piper.exe"),
            piper_voice: dir.join(voice.onnx.file_name),
        }
    }

    /// Whether `voice`'s model is already downloaded (its `.onnx` is present). The
    /// picker uses this to show which voices need a download before they can be
    /// selected; it does not check the shared whisper/piper.exe, which the pipeline
    /// provisions regardless of the voice.
    pub fn voice_installed(root: &Path, voice: &PiperVoice) -> bool {
        Self::dir(root).join(voice.onnx.file_name).is_file()
    }

    /// Return the assets for `voice` if the shared models and this voice's model
    /// are already present on disk — the offline / bundled-by-the-installer path,
    /// where nothing needs downloading.
    pub fn locate(root: &Path, voice: &PiperVoice) -> Option<Self> {
        let paths = Self::paths(root, voice);
        (paths.whisper_model.is_file() && paths.piper_exe.is_file() && paths.piper_voice.is_file())
            .then_some(paths)
    }

    /// Ensure every asset for `voice` is present and verified, downloading whatever
    /// is missing. Idempotent: an already-correct file is re-hashed and skipped, so
    /// this is cheap to call on every launch — and cheap on a voice *switch*, where
    /// the shared whisper/piper.exe are skipped and only the new voice downloads.
    pub async fn ensure(
        root: &Path,
        downloader: &Downloader,
        voice: &PiperVoice,
        progress: Option<AssetProgress>,
    ) -> Result<Self> {
        let dir = Self::dir(root);
        std::fs::create_dir_all(&dir)
            .map_err(|error| Error::io(format!("creating {}", dir.display()), error))?;

        // Whisper (shared) plus the selected voice's model and config.
        for asset in [WHISPER_MODEL, voice.onnx, voice.config] {
            fetch(downloader, &asset, &dir.join(asset.file_name), &progress).await?;
        }

        // The Piper binary bundle (shared): fetch the verified zip, extract once.
        let piper_dir = dir.join("piper");
        let piper_exe = piper_dir.join("piper.exe");
        if !piper_exe.is_file() {
            let archive = dir.join(PIPER_ARCHIVE.file_name);
            fetch(downloader, &PIPER_ARCHIVE, &archive, &progress).await?;
            extract_piper(&archive, &dir)?;
            if !piper_exe.is_file() {
                return Err(Error::model(
                    "the Piper archive did not contain piper/piper.exe",
                ));
            }
        }

        Ok(Self::paths(root, voice))
    }
}

/// Download one pinned asset to `dest`, verifying its digest.
async fn fetch(
    downloader: &Downloader,
    asset: &PinnedAsset,
    dest: &Path,
    progress: &Option<AssetProgress>,
) -> Result<()> {
    let sha256 = asset.digest()?;
    let progress_fn = progress.as_ref().map(|sink| {
        let sink = Arc::clone(sink);
        let label = asset.label;
        Arc::new(move |p: Progress| sink(label, p)) as ic_llama::download::ProgressFn
    });
    let request = DownloadRequest {
        url: asset.url.to_string(),
        dest: dest.to_path_buf(),
        sha256,
        progress: progress_fn,
    };
    downloader
        .fetch(&request)
        .await
        .map_err(|error| Error::model(format!("downloading {}: {error}", asset.label)))
}

/// Extract the Piper zip into `dest`, preserving its internal `piper/` layout, with
/// a zip-slip guard. Mirrors the extraction in `ic_llama::runtime`.
fn extract_piper(archive: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(archive)
        .map_err(|error| Error::io(format!("opening {}", archive.display()), error))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|error| Error::model(format!("reading the Piper archive: {error}")))?;

    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|error| Error::model(format!("reading a Piper archive entry: {error}")))?;
        // `enclosed_name` returns None for `..`/absolute paths — reject them.
        let Some(rel) = entry.enclosed_name() else {
            return Err(Error::model(format!(
                "unsafe path in the Piper archive: {}",
                entry.name()
            )));
        };
        let out = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)
                .map_err(|error| Error::io(format!("creating {}", out.display()), error))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| Error::io(format!("creating {}", parent.display()), error))?;
        }
        let mut writer = std::fs::File::create(&out)
            .map_err(|error| Error::io(format!("writing {}", out.display()), error))?;
        std::io::copy(&mut entry, &mut writer)
            .map_err(|error| Error::io(format!("extracting {}", out.display()), error))?;
    }
    Ok(())
}

/// List the bundled rustpotter wakeword models (`.rpw`) under `assets_dir`. These
/// ship in-tree (recorded by us), so there is nothing to download; an empty result
/// simply means wake-word is unavailable and the widget falls back to push-to-talk.
pub fn bundled_wake_models(assets_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(assets_dir) else {
        return Vec::new();
    };
    let mut models: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("rpw"))
        })
        .collect();
    models.sort();
    models
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pinned asset, shared and per-voice, for the blanket checks below.
    fn all_pins() -> Vec<PinnedAsset> {
        let mut pins = vec![WHISPER_MODEL, PIPER_ARCHIVE];
        for voice in VOICES {
            pins.push(voice.onnx);
            pins.push(voice.config);
        }
        pins
    }

    #[test]
    fn every_pinned_digest_parses() {
        // A malformed pin fails here rather than at download time on a user's
        // machine.
        for asset in all_pins() {
            asset
                .digest()
                .unwrap_or_else(|e| panic!("{}: {e}", asset.label));
        }
    }

    #[test]
    fn every_pinned_url_is_https_and_named() {
        for asset in all_pins() {
            assert!(asset.url.starts_with("https://"), "{}", asset.label);
            assert!(!asset.file_name.is_empty(), "{}", asset.label);
            assert!(asset.size_bytes > 0, "{}", asset.label);
        }
    }

    #[test]
    fn the_catalog_is_well_formed() {
        assert!(!VOICES.is_empty());
        // The default must be in the catalog, or `voice_or_default` cannot fall back.
        assert!(find_voice(DEFAULT_VOICE_ID).is_some());
        // Ids are unique, and each voice's onnx/config file names match its id — the
        // picker keys on the id, and two voices sharing a file name would collide on
        // disk (a switch would "download" a file that is already the other voice's).
        for voice in VOICES {
            assert_eq!(
                VOICES.iter().filter(|v| v.id == voice.id).count(),
                1,
                "{}",
                voice.id
            );
            assert!(voice.onnx.file_name.starts_with(voice.id), "{}", voice.id);
            assert!(voice.config.file_name.starts_with(voice.id), "{}", voice.id);
            assert!(
                voice.config.file_name.ends_with(".onnx.json"),
                "{}",
                voice.id
            );
        }
    }

    #[test]
    fn an_unknown_or_missing_voice_id_resolves_to_the_default() {
        assert_eq!(voice_or_default(None).id, DEFAULT_VOICE_ID);
        assert_eq!(voice_or_default(Some("no-such-voice")).id, DEFAULT_VOICE_ID);
        assert_eq!(
            voice_or_default(Some("en_GB-alan-medium")).id,
            "en_GB-alan-medium"
        );
    }

    #[test]
    fn locate_is_none_when_nothing_is_downloaded() {
        let tmp = std::env::temp_dir().join(format!("ic_voice_assets_{}", std::process::id()));
        let voice = voice_or_default(None);
        assert!(VoiceAssets::locate(&tmp, voice).is_none());
        assert!(!VoiceAssets::voice_installed(&tmp, voice));
    }

    #[test]
    fn bundled_wake_models_of_a_missing_dir_is_empty() {
        let missing = Path::new("no-such-wake-dir-xyz");
        assert!(bundled_wake_models(missing).is_empty());
    }
}
