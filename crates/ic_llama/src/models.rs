//! The local GGUF model store.
//!
//! ```text
//! <root>/models/Qwen3-4B-Q4_K_M.gguf          the weights
//! <root>/models/Qwen3-4B-Q4_K_M.suspect       why we stopped auto-loading it
//! ```
//!
//! A *suspect* marker is written when [`crate::server::Sidecar`] gives up on a
//! model after repeated crashes. It survives restarts on purpose: the model that
//! killed `llama-server` twice yesterday will kill it twice again today, and a
//! user who reopens the app should see an explanation rather than watch the same
//! restart loop from the beginning.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::download::{DownloadRequest, Downloader, ProgressFn};
use crate::error::{Error, Result};
use crate::gguf::GgufModel;
use crate::ids::{ModelId, Sha256Hex};

/// Suffix of the marker written beside a model that keeps crashing the server.
const SUSPECT_SUFFIX: &str = "suspect";

/// A GGUF file in a HuggingFace repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubModel {
    /// `owner/name`, e.g. `Qwen/Qwen3-4B-GGUF`.
    pub repo: String,
    /// Branch, tag, or commit. Pin a commit for reproducibility.
    pub revision: String,
    /// Path within the repo, e.g. `Qwen3-4B-Q4_K_M.gguf`.
    pub file: String,
}

impl HubModel {
    /// A model on the repository's default branch.
    pub fn new(repo: impl Into<String>, file: impl Into<String>) -> Self {
        Self {
            repo: repo.into(),
            revision: "main".into(),
            file: file.into(),
        }
    }

    /// Pin to an exact revision.
    pub fn at_revision(mut self, revision: impl Into<String>) -> Self {
        self.revision = revision.into();
        self
    }

    /// The download URL.
    pub fn url(&self) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.repo, self.revision, self.file
        )
    }

    /// The id this model is stored and served under, derived from its file name.
    pub fn model_id(&self) -> Result<ModelId> {
        let file_name = self.file.rsplit('/').next().unwrap_or(&self.file);
        ModelId::from_gguf_file_name(file_name)
    }
}

/// How to verify a downloaded model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Digest {
    /// A digest we recorded ahead of time. The strongest option: the bytes are
    /// checked against a value that did not come from the server serving them.
    Pinned(Sha256Hex),
    /// Ask the hub what the digest should be, then verify against that.
    ///
    /// This detects a truncated or corrupted transfer, not a malicious hub — the
    /// expected value and the bytes come from the same origin. Prefer
    /// [`Digest::Pinned`] for models shipped with the app.
    FromHub,
}

/// A model on disk.
#[derive(Debug, Clone)]
pub struct InstalledModel {
    /// What it is called.
    pub id: ModelId,
    /// Where the weights are.
    pub path: PathBuf,
    /// Its parsed header.
    pub gguf: GgufModel,
    /// Why it is not auto-loaded, when it isn't.
    pub suspect: Option<String>,
}

impl InstalledModel {
    /// Whether the model is safe to launch without the user re-confirming.
    pub fn is_loadable(&self) -> bool {
        self.suspect.is_none()
    }
}

/// The directory of downloaded models.
#[derive(Clone)]
pub struct ModelStore {
    dir: PathBuf,
    downloader: Downloader,
}

impl ModelStore {
    /// Open (or create on first write) the store under `<root>/models`.
    pub fn new(root: &Path, downloader: Downloader) -> Self {
        Self {
            dir: root.join("models"),
            downloader,
        }
    }

    /// Where a model's weights live.
    pub fn model_path(&self, id: &ModelId) -> PathBuf {
        self.dir.join(format!("{id}.gguf"))
    }

    fn suspect_path(&self, id: &ModelId) -> PathBuf {
        self.dir.join(format!("{id}.{SUSPECT_SUFFIX}"))
    }

    /// Every model in the store, sorted by id.
    ///
    /// Models whose header fails to parse are skipped with a warning rather than
    /// failing the whole listing — one bad download should not hide the user's
    /// other models from the picker.
    pub async fn list(&self) -> Result<Vec<InstalledModel>> {
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(Error::io(format!("listing {}", self.dir.display()), source));
            }
        };

        let mut models = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|source| Error::io(format!("listing {}", self.dir.display()), source))?
        {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "gguf") {
                continue;
            }
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let id = match ModelId::from_gguf_file_name(file_name) {
                Ok(id) => id,
                Err(error) => {
                    tracing::warn!(file = %file_name, %error, "skipping unusable model file name");
                    continue;
                }
            };
            match self.load(&id).await {
                Ok(model) => models.push(model),
                Err(error) => {
                    tracing::warn!(model = %id, %error, "skipping unreadable model");
                }
            }
        }
        models.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(models)
    }

    /// Read one model's header and suspect marker.
    pub async fn load(&self, id: &ModelId) -> Result<InstalledModel> {
        let path = self.model_path(id);
        let gguf = GgufModel::read(&path).await?;
        Ok(InstalledModel {
            suspect: self.suspect_reason(id).await?,
            id: id.clone(),
            path,
            gguf,
        })
    }

    /// Download `model` into the store unless a verified copy is already there.
    ///
    /// The download resumes across app restarts, so a user who quits during a
    /// 4 GB transfer does not start over.
    pub async fn install(
        &self,
        model: &HubModel,
        digest: Digest,
        progress: Option<ProgressFn>,
    ) -> Result<InstalledModel> {
        let id = model.model_id()?;
        let dest = self.model_path(&id);

        let sha256 = match digest {
            Digest::Pinned(digest) => digest,
            Digest::FromHub => self.fetch_hub_digest(model).await?,
        };

        self.downloader
            .fetch(&DownloadRequest {
                url: model.url(),
                dest: dest.clone(),
                sha256,
                progress,
            })
            .await?;

        // The bytes matched the digest, but a digest only proves we got what the
        // hub has. If it isn't a GGUF file, keeping it would make the model
        // picker show a model that can never load.
        let gguf = match GgufModel::read(&dest).await {
            Ok(gguf) => gguf,
            Err(error) => {
                if let Err(cleanup) = tokio::fs::remove_file(&dest).await {
                    tracing::warn!(path = %dest.display(), %cleanup, "could not remove the bad download");
                }
                return Err(error);
            }
        };

        // A freshly downloaded model has not crashed anything yet.
        self.clear_suspect(&id).await?;

        tracing::info!(
            model = %id,
            architecture = %gguf.architecture,
            blocks = gguf.block_count,
            bytes = gguf.file_size,
            "installed model"
        );
        Ok(InstalledModel {
            id,
            path: dest,
            gguf,
            suspect: None,
        })
    }

    /// Record that this model repeatedly took `llama-server` down.
    pub async fn mark_suspect(&self, id: &ModelId, reason: &str) -> Result<()> {
        let path = self.suspect_path(id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|source| {
                Error::io(format!("creating directory {}", parent.display()), source)
            })?;
        }
        tokio::fs::write(&path, reason)
            .await
            .map_err(|source| Error::io(format!("writing {}", path.display()), source))
    }

    /// Why the model is marked suspect, if it is.
    pub async fn suspect_reason(&self, id: &ModelId) -> Result<Option<String>> {
        let path = self.suspect_path(id);
        match tokio::fs::read_to_string(&path).await {
            Ok(reason) => Ok(Some(reason)),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(Error::io(format!("reading {}", path.display()), source)),
        }
    }

    /// Let the model be auto-loaded again — the user changed the context size,
    /// the GPU layers, or just wants to try once more.
    pub async fn clear_suspect(&self, id: &ModelId) -> Result<()> {
        let path = self.suspect_path(id);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(Error::io(format!("removing {}", path.display()), source)),
        }
    }

    /// Delete a model and its marker.
    pub async fn remove(&self, id: &ModelId) -> Result<()> {
        let path = self.model_path(id);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(Error::io(format!("removing {}", path.display()), source)),
        }
        self.clear_suspect(id).await
    }

    /// Ask the hub for a file's SHA-256.
    ///
    /// GGUF files are always stored in Git LFS, whose object id *is* the file's
    /// SHA-256. A file outside LFS reports a git blob SHA-1 instead, which we
    /// cannot use, so we refuse rather than skip verification.
    async fn fetch_hub_digest(&self, model: &HubModel) -> Result<Sha256Hex> {
        let url = format!(
            "https://huggingface.co/api/models/{}/paths-info/{}",
            model.repo, model.revision
        );
        let refuse = |reason: String| Error::HubResolve {
            repo: model.repo.clone(),
            revision: model.revision.clone(),
            file: model.file.clone(),
            reason,
        };

        let response = self
            .downloader
            .client()
            .post(&url)
            .json(&serde_json::json!({ "paths": [model.file] }))
            .send()
            .await
            .map_err(|source| Error::Http {
                url: url.clone(),
                source,
            })?;
        if !response.status().is_success() {
            return Err(Error::HttpStatus {
                url,
                status: response.status().as_u16(),
            });
        }

        let entries: Vec<PathInfo> = response.json().await.map_err(|source| Error::Http {
            url: url.clone(),
            source,
        })?;
        let entry = entries
            .into_iter()
            .find(|entry| entry.path == model.file)
            .ok_or_else(|| refuse("the hub does not list this file".into()))?;
        let lfs = entry.lfs.ok_or_else(|| {
            refuse(
                "the file is not stored in Git LFS, so the hub cannot supply a SHA-256; \
                 pin the digest explicitly"
                    .into(),
            )
        })?;
        Sha256Hex::parse_prefixed(&lfs.oid)
    }
}

/// One entry of the hub's `paths-info` response.
#[derive(Debug, Deserialize)]
struct PathInfo {
    path: String,
    lfs: Option<LfsInfo>,
}

#[derive(Debug, Deserialize)]
struct LfsInfo {
    /// The LFS object id, which for `sha256` (the only algorithm LFS uses) is
    /// the file's SHA-256.
    oid: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(root: &Path) -> ModelStore {
        ModelStore::new(root, Downloader::new().expect("client"))
    }

    #[test]
    fn hub_urls_carry_the_revision() {
        let model = HubModel::new("Qwen/Qwen3-4B-GGUF", "Qwen3-4B-Q4_K_M.gguf");
        assert_eq!(
            model.url(),
            "https://huggingface.co/Qwen/Qwen3-4B-GGUF/resolve/main/Qwen3-4B-Q4_K_M.gguf"
        );
        let pinned = model.at_revision("d4b1f2");
        assert!(pinned.url().contains("/resolve/d4b1f2/"));
    }

    #[test]
    fn the_model_id_comes_from_the_file_name_not_the_repo() {
        let model = HubModel::new("Qwen/Qwen3-4B-GGUF", "nested/dir/Qwen3-4B-Q4_K_M.gguf");
        assert_eq!(model.model_id().expect("valid").as_str(), "Qwen3-4B-Q4_K_M");
    }

    #[test]
    fn a_hub_path_can_never_place_a_model_outside_the_store() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = store(temp.path());

        // Only the file name is used, so the traversal segments are dropped
        // rather than followed...
        let model = HubModel::new("evil/repo", "../../../etc/passwd.gguf");
        let id = model.model_id().expect("the basename is a valid id");
        assert_eq!(id.as_str(), "passwd");
        assert_eq!(store.model_path(&id), store.dir.join("passwd.gguf"));

        // ...and a name that survives as a whole segment is rejected outright,
        // because `ModelId` allows neither separator.
        assert!(
            HubModel::new("evil/repo", "..\\..\\escape.gguf")
                .model_id()
                .is_err()
        );
        assert!(ModelId::new("../escape").is_err());
    }

    #[tokio::test]
    async fn listing_an_absent_store_is_empty_not_an_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let models = store(temp.path()).list().await.expect("list");
        assert!(models.is_empty());
    }

    #[tokio::test]
    async fn suspect_markers_round_trip_and_clear() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = store(temp.path());
        let id = ModelId::new("test-model").expect("valid");

        assert_eq!(store.suspect_reason(&id).await.expect("read"), None);

        store
            .mark_suspect(&id, "crashed twice")
            .await
            .expect("mark");
        assert_eq!(
            store.suspect_reason(&id).await.expect("read").as_deref(),
            Some("crashed twice")
        );

        store.clear_suspect(&id).await.expect("clear");
        assert_eq!(store.suspect_reason(&id).await.expect("read"), None);
        // Clearing an already-clear marker is not an error.
        store.clear_suspect(&id).await.expect("clear again");
    }

    #[tokio::test]
    async fn a_suspect_model_is_not_loadable() {
        let model = InstalledModel {
            id: ModelId::new("m").expect("valid"),
            path: PathBuf::from("m.gguf"),
            gguf: GgufModel {
                path: PathBuf::from("m.gguf"),
                file_size: 0,
                architecture: "test".into(),
                name: None,
                block_count: 1,
                context_length: None,
                embedding_length: None,
                head_count: None,
                head_count_kv: None,
                key_length: None,
                value_length: None,
                layer_bytes: vec![0],
                overhead_bytes: 0,
                metadata: Default::default(),
            },
            suspect: Some("crashed".into()),
        };
        assert!(!model.is_loadable());
    }

    #[tokio::test]
    async fn listing_skips_non_gguf_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("models");
        std::fs::create_dir_all(&dir).expect("create dir");
        std::fs::write(dir.join("notes.txt"), b"hello").expect("write");
        std::fs::write(dir.join("model.suspect"), b"reason").expect("write");

        let models = store(temp.path()).list().await.expect("list");
        assert!(models.is_empty());
    }

    #[tokio::test]
    async fn listing_skips_a_corrupt_model_instead_of_failing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("models");
        std::fs::create_dir_all(&dir).expect("create dir");
        std::fs::write(dir.join("broken.gguf"), b"not a gguf file").expect("write");

        let models = store(temp.path()).list().await.expect("list");
        assert!(models.is_empty());
    }
}
