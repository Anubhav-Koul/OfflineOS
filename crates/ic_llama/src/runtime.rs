//! Installing the pinned llama.cpp binaries.
//!
//! Layout under the caller's data directory:
//!
//! ```text
//! <root>/cache/llama-b9948-bin-win-vulkan-x64.zip   downloaded archives, verified
//! <root>/runtimes/b9948-vulkan/                     extracted, ready to run
//! <root>/runtimes/b9948-vulkan/.ic_llama_installed  written last; absence means "redo"
//! ```
//!
//! Nothing here trusts the archive's internal layout. Upstream has moved
//! `llama-server.exe` between the archive root and a `build/bin/` subdirectory
//! across releases, so we extract first and then *search* for the binary rather
//! than joining a hardcoded path that would break on the next pin bump.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::download::{DownloadRequest, Downloader, Progress};
use crate::error::{Error, Result};
use crate::release::{Asset, Backend, LLAMA_CPP_TAG, ensure_supported_platform};

/// Written once extraction succeeds. Its contents identify what was installed,
/// so a pin bump invalidates it even if the directory name were reused.
const MARKER: &str = ".ic_llama_installed";

/// How deep to search the extracted tree for the server binary.
const MAX_SEARCH_DEPTH: usize = 4;

/// Progress sink for an install. The `&str` is the archive currently being
/// fetched; extraction reports no progress (it is seconds, not minutes).
pub type InstallProgressFn = Arc<dyn Fn(&str, Progress) + Send + Sync>;

/// An installed, ready-to-run llama.cpp build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlamaRuntime {
    /// The directory the archives were extracted into.
    pub dir: PathBuf,
    /// Absolute path to `llama-server`. Its siblings are the backend DLLs, so
    /// it must be launched without relocating it.
    pub server_bin: PathBuf,
    /// Which build this is.
    pub backend: Backend,
}

impl LlamaRuntime {
    /// Ensure the pinned build for `backend` is installed under `root`, and
    /// return it.
    ///
    /// Idempotent and safe to call on every app start: an intact install is
    /// detected by its marker and returns without touching the network. An
    /// install interrupted partway through (no marker) is discarded and redone,
    /// because a half-extracted directory would otherwise be indistinguishable
    /// from a good one.
    pub async fn install(
        root: &Path,
        backend: Backend,
        downloader: &Downloader,
        progress: Option<InstallProgressFn>,
    ) -> Result<Self> {
        ensure_supported_platform()?;

        let dir = root.join("runtimes").join(install_dir_name(backend));
        if let Some(runtime) = Self::existing(&dir, backend).await? {
            tracing::debug!(dir = %dir.display(), "llama.cpp runtime already installed");
            return Ok(runtime);
        }

        // Either nothing is there or an earlier attempt died mid-extraction.
        remove_dir(&dir).await?;
        create_dir(&dir).await?;

        let cache = root.join("cache");
        for asset in backend.assets() {
            let archive = fetch_asset(downloader, asset, &cache, progress.clone()).await?;
            extract_into(&archive, &dir).await?;
        }

        let server_bin = find_server_binary(&dir).await?;
        write_marker(&dir, backend).await?;
        tracing::info!(
            tag = LLAMA_CPP_TAG,
            backend = backend.as_str(),
            server = %server_bin.display(),
            "installed llama.cpp runtime"
        );

        Ok(Self {
            dir,
            server_bin,
            backend,
        })
    }

    /// Return the install at `dir` if its marker matches the current pin.
    async fn existing(dir: &Path, backend: Backend) -> Result<Option<Self>> {
        let marker = dir.join(MARKER);
        let contents = match tokio::fs::read_to_string(&marker).await {
            Ok(contents) => contents,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(Error::io(format!("reading {}", marker.display()), source));
            }
        };
        if contents.trim() != marker_contents(backend) {
            return Ok(None);
        }
        // The marker can outlive the binary if a user or an antivirus product
        // removed it; treat that as "not installed" rather than handing back a
        // path that does not exist.
        match find_server_binary(dir).await {
            Ok(server_bin) => Ok(Some(Self {
                dir: dir.to_path_buf(),
                server_bin,
                backend,
            })),
            Err(Error::Archive { .. }) => Ok(None),
            Err(other) => Err(other),
        }
    }
}

/// `b9948-vulkan`.
fn install_dir_name(backend: Backend) -> String {
    format!("{LLAMA_CPP_TAG}-{}", backend.as_str())
}

fn marker_contents(backend: Backend) -> String {
    format!("{LLAMA_CPP_TAG} {}", backend.as_str())
}

/// Download `asset` into `cache` (or reuse a verified copy) and return its path.
async fn fetch_asset(
    downloader: &Downloader,
    asset: &Asset,
    cache: &Path,
    progress: Option<InstallProgressFn>,
) -> Result<PathBuf> {
    let dest = cache.join(asset.name);
    let name = asset.name.to_string();
    let request = DownloadRequest {
        url: asset.url(),
        dest: dest.clone(),
        sha256: asset.digest()?,
        progress: progress.map(|sink| {
            Arc::new(move |progress: Progress| sink(&name, progress)) as crate::download::ProgressFn
        }),
    };
    downloader.fetch(&request).await?;
    Ok(dest)
}

/// Unzip `archive` over `dir`.
///
/// Rejects entries whose paths escape `dir` — a zip-slip guard. The archives are
/// digest-pinned, so this cannot fire today; it is here so that a future
/// unpinned or user-supplied archive cannot write outside the runtime directory.
async fn extract_into(archive: &Path, dir: &Path) -> Result<()> {
    let archive = archive.to_path_buf();
    let dir = dir.to_path_buf();
    let handle = tokio::task::spawn_blocking(move || extract_blocking(&archive, &dir));
    match handle.await {
        Ok(result) => result,
        Err(join_error) => Err(Error::io(
            "archive extraction task failed",
            std::io::Error::other(join_error),
        )),
    }
}

fn extract_blocking(archive: &Path, dir: &Path) -> Result<()> {
    let corrupt = |reason: String| Error::Archive {
        path: archive.to_path_buf(),
        reason,
    };

    let file = std::fs::File::open(archive)
        .map_err(|source| Error::io(format!("opening {}", archive.display()), source))?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::with_capacity(1 << 20, file))
        .map_err(|error| corrupt(error.to_string()))?;

    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|error| corrupt(error.to_string()))?;

        // `enclosed_name` is `None` for absolute paths and for anything
        // containing `..`.
        let Some(relative) = entry.enclosed_name() else {
            return Err(corrupt(format!(
                "entry {:?} would extract outside the runtime directory",
                entry.name()
            )));
        };
        let target = dir.join(relative);

        if entry.is_dir() {
            std::fs::create_dir_all(&target).map_err(|source| {
                Error::io(format!("creating directory {}", target.display()), source)
            })?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|source| {
                Error::io(format!("creating directory {}", parent.display()), source)
            })?;
        }
        let mut out = std::fs::File::create(&target)
            .map_err(|source| Error::io(format!("creating {}", target.display()), source))?;
        std::io::copy(&mut entry, &mut out)
            .map_err(|source| Error::io(format!("extracting {}", target.display()), source))?;

        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode)).map_err(
                |source| {
                    Error::io(
                        format!("setting permissions on {}", target.display()),
                        source,
                    )
                },
            )?;
        }
    }
    Ok(())
}

/// The file name upstream gives the server binary.
fn server_binary_name() -> &'static str {
    if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    }
}

/// Breadth-first search for the server binary under `dir`.
async fn find_server_binary(dir: &Path) -> Result<PathBuf> {
    let name = server_binary_name();
    let mut frontier = vec![dir.to_path_buf()];

    for _ in 0..MAX_SEARCH_DEPTH {
        let mut next = Vec::new();
        for directory in frontier {
            let mut entries = match tokio::fs::read_dir(&directory).await {
                Ok(entries) => entries,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(Error::io(
                        format!("listing {}", directory.display()),
                        source,
                    ));
                }
            };
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|source| Error::io(format!("listing {}", directory.display()), source))?
            {
                let path = entry.path();
                let file_type = entry.file_type().await.map_err(|source| {
                    Error::io(format!("inspecting {}", path.display()), source)
                })?;
                if file_type.is_dir() {
                    next.push(path);
                } else if entry.file_name() == name {
                    return Ok(path);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }

    Err(Error::Archive {
        path: dir.to_path_buf(),
        reason: format!("no {name} found within {MAX_SEARCH_DEPTH} directory levels"),
    })
}

async fn write_marker(dir: &Path, backend: Backend) -> Result<()> {
    let marker = dir.join(MARKER);
    tokio::fs::write(&marker, marker_contents(backend))
        .await
        .map_err(|source| Error::io(format!("writing {}", marker.display()), source))
}

async fn create_dir(dir: &Path) -> Result<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|source| Error::io(format!("creating directory {}", dir.display()), source))
}

async fn remove_dir(dir: &Path) -> Result<()> {
    match tokio::fs::remove_dir_all(dir).await {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::io(
            format!("removing directory {}", dir.display()),
            source,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Build a zip in memory with the given entries.
    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buffer = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buffer));
            let options: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            for (name, contents) in entries {
                writer.start_file(*name, options).expect("start entry");
                writer.write_all(contents).expect("write entry");
            }
            writer.finish().expect("finish zip");
        }
        buffer
    }

    #[tokio::test]
    async fn extraction_reproduces_the_archive_tree() {
        let temp = tempfile::tempdir().expect("tempdir");
        let archive = temp.path().join("build.zip");
        std::fs::write(
            &archive,
            zip_with(&[
                ("build/bin/llama-server.exe", b"binary"),
                ("build/bin/ggml.dll", b"library"),
            ]),
        )
        .expect("write archive");

        let dir = temp.path().join("runtime");
        extract_into(&archive, &dir).await.expect("extract");

        assert_eq!(
            std::fs::read(dir.join("build/bin/llama-server.exe")).expect("extracted binary"),
            b"binary"
        );
    }

    #[tokio::test]
    async fn extraction_rejects_paths_that_escape_the_runtime_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let archive = temp.path().join("evil.zip");
        std::fs::write(&archive, zip_with(&[("../escaped.txt", b"pwned")])).expect("write archive");

        let dir = temp.path().join("runtime");
        let error = extract_into(&archive, &dir)
            .await
            .expect_err("zip slip must be rejected");
        assert!(matches!(error, Error::Archive { .. }), "{error:?}");
        assert!(!temp.path().join("escaped.txt").exists());
    }

    #[tokio::test]
    async fn the_server_binary_is_found_wherever_the_archive_put_it() {
        let temp = tempfile::tempdir().expect("tempdir");
        let nested = temp.path().join("build").join("bin");
        std::fs::create_dir_all(&nested).expect("create nested dir");
        let expected = nested.join(server_binary_name());
        std::fs::write(&expected, b"binary").expect("write binary");

        let found = find_server_binary(temp.path()).await.expect("found");
        assert_eq!(found, expected);
    }

    #[tokio::test]
    async fn a_runtime_without_a_server_binary_is_an_archive_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("ggml.dll"), b"library").expect("write file");
        let error = find_server_binary(temp.path())
            .await
            .expect_err("no server binary");
        assert!(matches!(error, Error::Archive { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn an_install_whose_marker_is_missing_is_not_reused() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("b9948-vulkan");
        std::fs::create_dir_all(&dir).expect("create dir");
        std::fs::write(dir.join(server_binary_name()), b"binary").expect("write binary");

        assert!(
            LlamaRuntime::existing(&dir, Backend::Vulkan)
                .await
                .expect("check")
                .is_none()
        );

        write_marker(&dir, Backend::Vulkan).await.expect("marker");
        let runtime = LlamaRuntime::existing(&dir, Backend::Vulkan)
            .await
            .expect("check")
            .expect("now installed");
        assert_eq!(runtime.backend, Backend::Vulkan);
    }

    #[tokio::test]
    async fn a_marker_from_a_different_backend_is_not_reused() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("b9948-vulkan");
        std::fs::create_dir_all(&dir).expect("create dir");
        std::fs::write(dir.join(server_binary_name()), b"binary").expect("write binary");
        write_marker(&dir, Backend::Cpu).await.expect("marker");

        assert!(
            LlamaRuntime::existing(&dir, Backend::Vulkan)
                .await
                .expect("check")
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_marker_without_the_binary_is_not_reused() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("b9948-vulkan");
        std::fs::create_dir_all(&dir).expect("create dir");
        write_marker(&dir, Backend::Vulkan).await.expect("marker");

        assert!(
            LlamaRuntime::existing(&dir, Backend::Vulkan)
                .await
                .expect("check")
                .is_none()
        );
    }
}
