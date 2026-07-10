//! The pinned llama.cpp release.
//!
//! We ship against one exact upstream build rather than "latest": llama.cpp
//! tags several times a day, its GGUF and server-API behavior moves with it, and
//! a desktop user should never get a different inference engine than the one we
//! tested. Bumping the pin is a deliberate, reviewed change — see
//! `docs/desktop/llama-cpp-pin.md` for the procedure.
//!
//! Digests are the `sha256` values GitHub publishes for each release asset, so
//! they can be re-derived from the API rather than taken on trust from whoever
//! last edited this file:
//!
//! ```text
//! gh api repos/ggml-org/llama.cpp/releases/tags/b9948 \
//!   --jq '.assets[] | select(.name | test("win-(vulkan|cpu|cuda-12.4)-x64|^cudart")) | "\(.name) \(.digest)"'
//! ```

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::hardware::GpuAdapter;
use crate::ids::Sha256Hex;

/// The upstream release tag every binary in this crate comes from.
pub const LLAMA_CPP_TAG: &str = "b9948";

/// Which llama.cpp build to run.
///
/// Vulkan is the default because one 33 MB archive covers NVIDIA, AMD, and Intel
/// with no runtime to install. CUDA is measurably faster on NVIDIA but costs a
/// 660 MB download (the build plus the CUDA runtime redistributable), so it is
/// opt-in rather than auto-selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// Vulkan compute. Works on any GPU with a modern driver.
    Vulkan,
    /// CUDA 12.4. NVIDIA only, and pulls the CUDA runtime alongside it.
    Cuda12,
    /// CPU only. The fallback when no usable GPU is present.
    Cpu,
}

impl Backend {
    /// Directory-name form, also the value persisted in the install marker.
    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::Vulkan => "vulkan",
            Backend::Cuda12 => "cuda12",
            Backend::Cpu => "cpu",
        }
    }

    /// The archives that make up this build. More than one for CUDA, which
    /// needs the runtime redistributable extracted alongside the binaries.
    pub fn assets(&self) -> &'static [Asset] {
        match self {
            Backend::Vulkan => VULKAN_ASSETS,
            Backend::Cuda12 => CUDA12_ASSETS,
            Backend::Cpu => CPU_ASSETS,
        }
    }

    /// Total bytes to download for this backend, for a pre-flight disk check.
    pub fn download_bytes(&self) -> u64 {
        self.assets().iter().map(|asset| asset.size_bytes).sum()
    }

    /// Pick a backend for the detected hardware.
    ///
    /// Never returns [`Backend::Cuda12`]: switching to CUDA is a settings
    /// decision the user makes knowing it costs a 660 MB download, not something
    /// we do behind their back on first run.
    pub fn recommended_for(gpus: &[GpuAdapter]) -> Backend {
        if gpus.iter().any(GpuAdapter::is_discrete) {
            Backend::Vulkan
        } else {
            Backend::Cpu
        }
    }
}

/// One release archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Asset {
    /// File name, which is also the name under the release tag.
    pub name: &'static str,
    /// Hex SHA-256, from the GitHub release API's `digest` field.
    pub sha256: &'static str,
    /// Size in bytes, for progress reporting and disk pre-flight.
    pub size_bytes: u64,
}

impl Asset {
    /// Where to download this asset from.
    pub fn url(&self) -> String {
        format!(
            "https://github.com/ggml-org/llama.cpp/releases/download/{LLAMA_CPP_TAG}/{}",
            self.name
        )
    }

    /// The pinned digest as a validated value.
    ///
    /// The table below is a compile-time constant, so a malformed digest is a
    /// bug in this file rather than untrusted input; the unit test at the bottom
    /// checks every entry so the failure surfaces at `cargo test` rather than at
    /// a user's first download.
    pub fn digest(&self) -> Result<Sha256Hex> {
        Sha256Hex::new(self.sha256)
    }
}

const VULKAN_ASSETS: &[Asset] = &[Asset {
    name: "llama-b9948-bin-win-vulkan-x64.zip",
    sha256: "18d1c0d56792e6a9f5082d4343c2431617cd2914243bafdac852758240bb9bfa",
    size_bytes: 32_907_039,
}];

const CPU_ASSETS: &[Asset] = &[Asset {
    name: "llama-b9948-bin-win-cpu-x64.zip",
    sha256: "b776d0e5b5360db4a26965dc3befbf184804795a8016815d99053c8ebfb10982",
    size_bytes: 18_219_242,
}];

/// The CUDA build does not bundle the CUDA runtime; upstream ships it as a
/// separate archive that must be extracted into the same directory.
const CUDA12_ASSETS: &[Asset] = &[
    Asset {
        name: "llama-b9948-bin-win-cuda-12.4-x64.zip",
        sha256: "27118c5faf4a4cc74708729a536fbeab939bdb3c48b7033404198c5c433c8f66",
        size_bytes: 267_349_334,
    },
    Asset {
        name: "cudart-llama-bin-win-cuda-12.4-x64.zip",
        sha256: "8c79a9b226de4b3cacfd1f83d24f962d0773be79f1e7b75c6af4ded7e32ae1d6",
        size_bytes: 391_443_627,
    },
];

/// The pinned archives are Windows x64 only.
pub(crate) fn ensure_supported_platform() -> Result<()> {
    if cfg!(all(windows, target_arch = "x86_64")) {
        Ok(())
    } else {
        Err(Error::UnsupportedPlatform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pinned_digest_is_a_valid_sha256() {
        for backend in [Backend::Vulkan, Backend::Cuda12, Backend::Cpu] {
            for asset in backend.assets() {
                asset
                    .digest()
                    .unwrap_or_else(|error| panic!("{}: {error}", asset.name));
            }
        }
    }

    #[test]
    fn every_asset_name_carries_the_pinned_tag() {
        for backend in [Backend::Vulkan, Backend::Cpu] {
            for asset in backend.assets() {
                assert!(
                    asset.name.contains(LLAMA_CPP_TAG),
                    "{} does not name tag {LLAMA_CPP_TAG}",
                    asset.name
                );
                assert!(asset.url().ends_with(asset.name));
            }
        }
    }

    #[test]
    fn cuda_is_never_auto_selected() {
        let discrete = GpuAdapter {
            name: "NVIDIA GeForce RTX 4070".into(),
            dedicated_vram_bytes: 12 << 30,
            budget_bytes: 11 << 30,
            used_bytes: 0,
        };
        assert_eq!(Backend::recommended_for(&[discrete]), Backend::Vulkan);
        assert_eq!(Backend::recommended_for(&[]), Backend::Cpu);
    }

    #[test]
    fn integrated_only_machines_get_the_cpu_build() {
        let integrated = GpuAdapter {
            name: "Intel(R) UHD Graphics".into(),
            dedicated_vram_bytes: 0,
            budget_bytes: 8 << 30,
            used_bytes: 0,
        };
        assert_eq!(Backend::recommended_for(&[integrated]), Backend::Cpu);

        // An APU's BIOS carve-out is not a reason to download the Vulkan build.
        let apu = GpuAdapter {
            name: "AMD Radeon(TM) Graphics".into(),
            dedicated_vram_bytes: 485 << 20,
            budget_bytes: 15 << 30,
            used_bytes: 0,
        };
        assert_eq!(Backend::recommended_for(&[apu]), Backend::Cpu);
    }

    #[test]
    fn cuda_pulls_the_runtime_redistributable_too() {
        let names: Vec<_> = Backend::Cuda12
            .assets()
            .iter()
            .map(|asset| asset.name)
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names.iter().any(|name| name.starts_with("cudart-")));
    }
}
