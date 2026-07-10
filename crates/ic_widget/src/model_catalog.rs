//! A small curated list of GGUF models the download UI suggests.
//!
//! Deliberately short and verified rather than broad and guessed: a wrong
//! `repo`/`file` here would 404 at download time. Entries the app has actually
//! run belong here; everything else is reachable through the panel's custom
//! repo/file field, which accepts any HuggingFace GGUF.
//!
//! Downloads verify against the digest HuggingFace reports (`Digest::FromHub`),
//! so an entry needs no pinned checksum — only a correct repo and file name.

use serde::Serialize;

/// One suggested model, as the download panel renders it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecommendedModel {
    /// The id it installs as — the file name without `.gguf`. Matches what the
    /// model store and the local-model panel call it.
    pub id: String,
    /// A human label for the card.
    pub name: String,
    /// HuggingFace `owner/name`.
    pub repo: String,
    /// File within the repo.
    pub file: String,
    /// Parameter count, for the card (e.g. `4B`).
    pub params: String,
    /// Quantization, for the card (e.g. `Q4_K_M`).
    pub quant: String,
    /// Rough download size in GiB, for the card. Not authoritative — the real
    /// size comes from the server once the transfer starts.
    pub approx_gib: f32,
    /// One-line note shown under the card.
    pub note: String,
}

/// The suggested models, best first.
pub fn recommended() -> Vec<RecommendedModel> {
    vec![RecommendedModel {
        id: "Qwen3-4B-Q4_K_M".to_string(),
        name: "Qwen3 4B".to_string(),
        repo: "Qwen/Qwen3-4B-GGUF".to_string(),
        file: "Qwen3-4B-Q4_K_M.gguf".to_string(),
        params: "4B".to_string(),
        quant: "Q4_K_M".to_string(),
        approx_gib: 2.5,
        note: "Verified default — full GPU offload on ~8 GB VRAM. The repo has \
               other quants (Q5_K_M, Q6_K, Q8_0); enter one below for a larger, \
               higher-quality file."
            .to_string(),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_recommended_entry_is_well_formed() {
        let models = recommended();
        assert!(!models.is_empty(), "the panel would show an empty list");
        for model in models {
            // A malformed repo or file would 404 at download time — catch the
            // obvious shape errors here instead.
            assert!(
                model.repo.contains('/'),
                "{} repo must be owner/name",
                model.id
            );
            assert!(
                model.file.ends_with(".gguf"),
                "{} file must be a .gguf",
                model.id
            );
            // The id is the file stem; the store derives the same thing, so a
            // mismatch would make an installed model look un-downloaded.
            assert_eq!(
                model.file,
                format!("{}.gguf", model.id),
                "{} id must match its file stem",
                model.id
            );
            assert!(model.approx_gib > 0.0);
        }
    }
}
