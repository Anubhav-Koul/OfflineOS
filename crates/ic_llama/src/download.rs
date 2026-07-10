//! Resumable, checksum-verified downloads.
//!
//! Both things this crate fetches are large and are fetched over a desktop
//! user's home connection: llama.cpp release archives (18 MB – 400 MB) and GGUF
//! model weights (often several GB). So every download:
//!
//! - streams into a sibling `<dest>.part` file and only renames into place once
//!   the digest matches, meaning a `dest` that exists is always a complete,
//!   verified file;
//! - resumes an interrupted transfer with an HTTP `Range` request, and copes
//!   with servers that ignore it (`200` instead of `206`) or that consider the
//!   partial file already complete (`416`);
//! - deletes the `.part` on a digest mismatch, because a corrupt prefix would
//!   otherwise make every subsequent resume fail the same way forever.
//!
//! Digests are verified by hashing the finished file rather than by hashing the
//! stream, since a resumed transfer cannot reconstruct the hasher state from
//! the previous process.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use reqwest::StatusCode;
use reqwest::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;

use crate::error::{Error, Result};
use crate::ids::Sha256Hex;

/// How much must be written before the next progress callback fires. Chunks off
/// the wire are a few KiB each; reporting every one of them would mean ~50k
/// callbacks for a 400 MB archive.
const PROGRESS_STRIDE_BYTES: u64 = 256 * 1024;

/// How often progress is reported even when the stride hasn't been reached, so
/// a slow connection still shows movement.
const PROGRESS_STRIDE_INTERVAL: Duration = Duration::from_millis(500);

/// A snapshot of a download in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Bytes on disk so far, including any bytes carried over from a resumed
    /// transfer.
    pub downloaded: u64,
    /// Total size, when the server told us. `None` for chunked responses.
    pub total: Option<u64>,
}

impl Progress {
    /// Completion in `0.0..=1.0`, or `None` when the total is unknown.
    pub fn fraction(&self) -> Option<f64> {
        let total = self.total?;
        if total == 0 {
            return None;
        }
        Some((self.downloaded as f64 / total as f64).clamp(0.0, 1.0))
    }
}

/// Progress sink. Called from the download task, so it must not block.
pub type ProgressFn = Arc<dyn Fn(Progress) + Send + Sync>;

/// One artifact to fetch.
pub struct DownloadRequest {
    /// Where to fetch from.
    pub url: String,
    /// Final path. Its parent directory is created if missing.
    pub dest: PathBuf,
    /// Expected digest of the complete file.
    pub sha256: Sha256Hex,
    /// Optional progress sink.
    pub progress: Option<ProgressFn>,
}

/// Fetches artifacts with resume + verification.
#[derive(Clone)]
pub struct Downloader {
    client: reqwest::Client,
}

impl Downloader {
    /// Build a downloader with a client tuned for large, slow transfers: no
    /// overall request timeout (a multi-GB model legitimately takes a long
    /// time), but a read timeout so a silently dead connection doesn't hang
    /// forever.
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .read_timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(30))
            .user_agent(concat!("ic_llama/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(Error::ClientInit)?;
        Ok(Self { client })
    }

    /// Reuse an existing client (tests, or a caller that configures its own
    /// proxy settings).
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }

    /// The underlying client, for callers that need a plain request (e.g. the
    /// HuggingFace metadata API).
    pub(crate) fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Fetch `request.url` into `request.dest`, resuming if a `.part` file from
    /// an earlier attempt is present.
    ///
    /// If `dest` already exists it is verified against the digest and, when it
    /// matches, returned untouched — this is what makes the installer and model
    /// store idempotent. A pre-existing `dest` whose digest does *not* match is
    /// treated as corrupt and re-downloaded.
    pub async fn fetch(&self, request: &DownloadRequest) -> Result<()> {
        if let Some(parent) = request.dest.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|source| {
                Error::io(format!("creating directory {}", parent.display()), source)
            })?;
        }

        if tokio::fs::try_exists(&request.dest).await.unwrap_or(false) {
            match verify_digest(&request.dest, &request.sha256).await {
                Ok(()) => {
                    tracing::debug!(dest = %request.dest.display(), "already downloaded and verified");
                    return Ok(());
                }
                Err(Error::ChecksumMismatch { actual, .. }) => {
                    tracing::warn!(
                        dest = %request.dest.display(),
                        expected = %request.sha256,
                        %actual,
                        "existing file failed verification; re-downloading"
                    );
                    remove_file(&request.dest).await?;
                }
                Err(other) => return Err(other),
            }
        }

        let part = part_path(&request.dest);
        self.stream_to_part(request, &part).await?;

        if let Err(error) = verify_digest(&part, &request.sha256).await {
            // A corrupt prefix poisons every future resume, so the `.part` has
            // to go even though the caller may retry immediately.
            remove_file(&part).await?;
            return Err(match error {
                Error::ChecksumMismatch {
                    expected, actual, ..
                } => Error::ChecksumMismatch {
                    url: request.url.clone(),
                    expected,
                    actual,
                },
                other => other,
            });
        }

        rename_over(&part, &request.dest).await
    }

    /// Drive the transfer into `part`, resuming when possible. On return `part`
    /// holds the complete (but not yet verified) body.
    async fn stream_to_part(&self, request: &DownloadRequest, part: &Path) -> Result<()> {
        let mut resume_from = file_len(part).await?;

        // At most two passes: the second only happens when the server rejects
        // our range, and it always starts from zero, so this cannot loop.
        let (response, resuming) = loop {
            let response = self.send(&request.url, resume_from).await?;
            let status = response.status();
            match status {
                StatusCode::PARTIAL_CONTENT if resume_from > 0 => {
                    if content_range_start(&response) == Some(resume_from) {
                        break (response, true);
                    }
                    tracing::warn!(
                        url = %request.url,
                        "server returned a range we did not ask for; restarting download"
                    );
                    resume_from = 0;
                }
                StatusCode::OK => {
                    // Either a fresh download, or the server ignored `Range`.
                    resume_from = 0;
                    break (response, false);
                }
                StatusCode::RANGE_NOT_SATISFIABLE if resume_from > 0 => {
                    // Our `.part` is at least as long as the resource: it is
                    // stale (the artifact changed) or truncated-to-longer by a
                    // previous bug. Start over.
                    tracing::warn!(
                        url = %request.url,
                        bytes = resume_from,
                        "server rejected the resume offset; restarting download"
                    );
                    remove_file(part).await?;
                    resume_from = 0;
                }
                _ => {
                    return Err(Error::HttpStatus {
                        url: request.url.clone(),
                        status: status.as_u16(),
                    });
                }
            }
        };

        if resume_from > 0 && !accepts_ranges(&response) {
            // Defensive: a 206 without `Accept-Ranges` is legal, but if the
            // header is present and says `none` we do not trust the offset.
            tracing::warn!(url = %request.url, "server does not accept ranges; restarting download");
            resume_from = 0;
        }

        let total = body_total(&response, resume_from);
        let mut file = open_part(part, resume_from > 0).await?;
        let mut downloaded = resume_from;
        let mut reporter = ProgressReporter::new(request.progress.clone(), downloaded, total);
        reporter.force(downloaded);

        if resuming {
            tracing::info!(url = %request.url, resume_from, ?total, "resuming download");
        } else {
            tracing::info!(url = %request.url, ?total, "starting download");
        }

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|source| Error::Http {
                url: request.url.clone(),
                source,
            })?;
            file.write_all(&chunk)
                .await
                .map_err(|source| Error::io(format!("writing to {}", part.display()), source))?;
            downloaded += chunk.len() as u64;
            reporter.maybe_report(downloaded);
        }

        file.flush()
            .await
            .map_err(|source| Error::io(format!("flushing {}", part.display()), source))?;
        file.sync_all()
            .await
            .map_err(|source| Error::io(format!("syncing {}", part.display()), source))?;
        reporter.force(downloaded);

        if let Some(expected) = total
            && downloaded != expected
        {
            return Err(Error::io(
                format!(
                    "download of {} ended early: got {downloaded} of {expected} bytes",
                    request.url
                ),
                std::io::Error::from(std::io::ErrorKind::UnexpectedEof),
            ));
        }

        Ok(())
    }

    async fn send(&self, url: &str, resume_from: u64) -> Result<reqwest::Response> {
        let mut builder = self.client.get(url);
        if resume_from > 0 {
            builder = builder.header(RANGE, format!("bytes={resume_from}-"));
        }
        builder.send().await.map_err(|source| Error::Http {
            url: url.to_string(),
            source,
        })
    }
}

/// Emits progress at most every [`PROGRESS_STRIDE_BYTES`] or
/// [`PROGRESS_STRIDE_INTERVAL`], whichever comes first.
struct ProgressReporter {
    sink: Option<ProgressFn>,
    total: Option<u64>,
    last_bytes: u64,
    last_at: Instant,
}

impl ProgressReporter {
    fn new(sink: Option<ProgressFn>, downloaded: u64, total: Option<u64>) -> Self {
        Self {
            sink,
            total,
            last_bytes: downloaded,
            last_at: Instant::now(),
        }
    }

    fn maybe_report(&mut self, downloaded: u64) {
        let by_bytes = downloaded.saturating_sub(self.last_bytes) >= PROGRESS_STRIDE_BYTES;
        let by_time = self.last_at.elapsed() >= PROGRESS_STRIDE_INTERVAL;
        if by_bytes || by_time {
            self.force(downloaded);
        }
    }

    fn force(&mut self, downloaded: u64) {
        self.last_bytes = downloaded;
        self.last_at = Instant::now();
        if let Some(sink) = &self.sink {
            sink(Progress {
                downloaded,
                total: self.total,
            });
        }
    }
}

/// `foo.gguf` → `foo.gguf.part`. Appends rather than replacing the extension so
/// two artifacts differing only by extension can't share a `.part`.
fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

async fn file_len(path: &Path) -> Result<u64> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) => Ok(metadata.len()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(source) => Err(Error::io(format!("inspecting {}", path.display()), source)),
    }
}

async fn open_part(path: &Path, append: bool) -> Result<tokio::fs::File> {
    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).write(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    options
        .open(path)
        .await
        .map_err(|source| Error::io(format!("opening {}", path.display()), source))
}

async fn remove_file(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::io(format!("removing {}", path.display()), source)),
    }
}

/// `rename` fails on Windows when the destination exists, so clear it first.
async fn rename_over(from: &Path, to: &Path) -> Result<()> {
    remove_file(to).await?;
    tokio::fs::rename(from, to).await.map_err(|source| {
        Error::io(
            format!("moving {} into place at {}", from.display(), to.display()),
            source,
        )
    })
}

/// Total size of the finished file, accounting for the bytes we already had.
fn body_total(response: &reqwest::Response, resume_from: u64) -> Option<u64> {
    if let Some(total) = content_range_total(response) {
        return Some(total);
    }
    let remaining = response
        .headers()
        .get(CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()?;
    Some(resume_from + remaining)
}

/// Parse `Content-Range: bytes 100-999/1000` → start `100`.
fn content_range_start(response: &reqwest::Response) -> Option<u64> {
    let value = response.headers().get(CONTENT_RANGE)?.to_str().ok()?;
    let range = value.strip_prefix("bytes ")?.split('/').next()?;
    range.split('-').next()?.trim().parse().ok()
}

/// Parse `Content-Range: bytes 100-999/1000` → total `1000`.
fn content_range_total(response: &reqwest::Response) -> Option<u64> {
    let value = response.headers().get(CONTENT_RANGE)?.to_str().ok()?;
    let total = value.rsplit('/').next()?.trim();
    total.parse().ok()
}

/// `false` only when the server explicitly says `Accept-Ranges: none`.
fn accepts_ranges(response: &reqwest::Response) -> bool {
    match response.headers().get(ACCEPT_RANGES) {
        Some(value) => !value.as_bytes().eq_ignore_ascii_case(b"none"),
        None => true,
    }
}

/// Hash `path` and compare against `expected`. Runs on a blocking thread: a
/// multi-gigabyte model would otherwise stall the async runtime for seconds.
pub(crate) async fn verify_digest(path: &Path, expected: &Sha256Hex) -> Result<()> {
    let actual = sha256_file(path).await?;
    if actual == expected.as_str() {
        Ok(())
    } else {
        Err(Error::ChecksumMismatch {
            url: path.display().to_string(),
            expected: expected.to_string(),
            actual,
        })
    }
}

/// Lowercase hex SHA-256 of a file's contents.
pub(crate) async fn sha256_file(path: &Path) -> Result<String> {
    let path = path.to_path_buf();
    let handle = tokio::task::spawn_blocking(move || -> Result<String> {
        let file = std::fs::File::open(&path)
            .map_err(|source| Error::io(format!("opening {}", path.display()), source))?;
        let mut reader = std::io::BufReader::with_capacity(1 << 20, file);
        let mut hasher = Sha256::new();
        std::io::copy(&mut reader, &mut hasher)
            .map_err(|source| Error::io(format!("hashing {}", path.display()), source))?;
        Ok(hex::encode(hasher.finalize()))
    });
    match handle.await {
        Ok(result) => result,
        Err(join_error) => Err(Error::io(
            "hashing task failed",
            std::io::Error::other(join_error),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_path_appends_rather_than_replaces_extension() {
        assert_eq!(
            part_path(Path::new("/models/foo.gguf")),
            PathBuf::from("/models/foo.gguf.part")
        );
    }

    #[test]
    fn progress_fraction_is_clamped_and_guards_zero_total() {
        assert_eq!(
            Progress {
                downloaded: 50,
                total: Some(100)
            }
            .fraction(),
            Some(0.5)
        );
        assert_eq!(
            Progress {
                downloaded: 5,
                total: Some(0)
            }
            .fraction(),
            None
        );
        assert_eq!(
            Progress {
                downloaded: 5,
                total: None
            }
            .fraction(),
            None
        );
    }
}
