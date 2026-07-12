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
//! - whisper `ggml-base.en.bin` — HuggingFace `ggerganov/whisper.cpp` (MIT).
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

/// whisper `base.en`, q?_0 GGML (147 MB). CPU-friendly, English-only.
pub const WHISPER_MODEL: PinnedAsset = PinnedAsset {
    label: "whisper base.en",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin",
    sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
    size_bytes: 147_964_211,
    file_name: "ggml-base.en.bin",
};

/// The Piper voice model (`en_US-amy-medium`, 63 MB).
pub const PIPER_VOICE: PinnedAsset = PinnedAsset {
    label: "Piper voice (amy)",
    url: "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/amy/medium/en_US-amy-medium.onnx",
    sha256: "b3a6e47b57b8c7fbe6a0ce2518161a50f59a9cdd8a50835c02cb02bdd6206c18",
    size_bytes: 63_201_294,
    file_name: "en_US-amy-medium.onnx",
};

/// The Piper voice config (sits beside the model; Piper reads it automatically).
pub const PIPER_VOICE_CONFIG: PinnedAsset = PinnedAsset {
    label: "Piper voice config",
    url: "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/amy/medium/en_US-amy-medium.onnx.json",
    sha256: "95a23eb4d42909d38df73bb9ac7f45f597dbfcde2d1bf9526fdeaf5466977d77",
    size_bytes: 4_882,
    file_name: "en_US-amy-medium.onnx.json",
};

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

    /// The paths the assets *would* occupy, without checking they exist.
    fn paths(root: &Path) -> Self {
        let dir = Self::dir(root);
        Self {
            whisper_model: dir.join(WHISPER_MODEL.file_name),
            // The archive extracts to `piper/piper.exe` under the voice dir.
            piper_exe: dir.join("piper").join("piper.exe"),
            piper_voice: dir.join(PIPER_VOICE.file_name),
        }
    }

    /// Return the assets if all three are already present on disk — the offline /
    /// bundled-by-the-installer path, where nothing needs downloading.
    pub fn locate(root: &Path) -> Option<Self> {
        let paths = Self::paths(root);
        (paths.whisper_model.is_file() && paths.piper_exe.is_file() && paths.piper_voice.is_file())
            .then_some(paths)
    }

    /// Ensure every asset is present and verified, downloading whatever is missing.
    /// Idempotent: an already-correct file is re-hashed and skipped, so this is
    /// cheap to call on every launch.
    pub async fn ensure(
        root: &Path,
        downloader: &Downloader,
        progress: Option<AssetProgress>,
    ) -> Result<Self> {
        let dir = Self::dir(root);
        std::fs::create_dir_all(&dir)
            .map_err(|error| Error::io(format!("creating {}", dir.display()), error))?;

        // The three single files.
        for asset in [WHISPER_MODEL, PIPER_VOICE, PIPER_VOICE_CONFIG] {
            fetch(downloader, &asset, &dir.join(asset.file_name), &progress).await?;
        }

        // The Piper binary bundle: fetch the verified zip, then extract it once.
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

        Ok(Self::paths(root))
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

    #[test]
    fn every_pinned_digest_parses() {
        // A malformed pin fails here rather than at download time on a user's
        // machine.
        for asset in [
            WHISPER_MODEL,
            PIPER_VOICE,
            PIPER_VOICE_CONFIG,
            PIPER_ARCHIVE,
        ] {
            asset
                .digest()
                .unwrap_or_else(|e| panic!("{}: {e}", asset.label));
        }
    }

    #[test]
    fn every_pinned_url_is_https_and_named() {
        for asset in [
            WHISPER_MODEL,
            PIPER_VOICE,
            PIPER_VOICE_CONFIG,
            PIPER_ARCHIVE,
        ] {
            assert!(asset.url.starts_with("https://"), "{}", asset.label);
            assert!(!asset.file_name.is_empty(), "{}", asset.label);
            assert!(asset.size_bytes > 0, "{}", asset.label);
        }
    }

    #[test]
    fn locate_is_none_when_nothing_is_downloaded() {
        let tmp = std::env::temp_dir().join(format!("ic_voice_assets_{}", std::process::id()));
        assert!(VoiceAssets::locate(&tmp).is_none());
    }

    #[test]
    fn bundled_wake_models_of_a_missing_dir_is_empty() {
        let missing = Path::new("no-such-wake-dir-xyz");
        assert!(bundled_wake_models(missing).is_empty());
    }
}
