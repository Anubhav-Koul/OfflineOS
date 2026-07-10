//! Download resume, verification, and the ways servers get `Range` wrong.
//!
//! A desktop user downloading a 4 GB model over a home connection *will* be
//! interrupted, so resume is not an optimization — it is the difference between
//! a model that installs and one that never does. These tests drive a mock
//! origin through each behavior a real CDN exhibits.

use std::sync::{Arc, Mutex};

use ic_llama::Error;
use ic_llama::download::{DownloadRequest, Downloader, Progress};
use ic_llama::ids::Sha256Hex;
use sha2::Digest as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// How the mock origin treats a `Range` header.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RangeSupport {
    /// The well-behaved case: `206` with only the requested tail.
    Honor,
    /// Plenty of origins (and every transparent proxy that decompresses) ignore
    /// `Range` and send the whole body with `200`.
    Ignore,
    /// The object changed and is now shorter than our partial file.
    Reject416,
}

/// A single-purpose HTTP origin serving one body.
struct Origin {
    port: u16,
    /// Every `Range` header value the origin saw, in order. `None` for a request
    /// that carried no `Range`.
    ranges: Arc<Mutex<Vec<Option<String>>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl Origin {
    async fn start(body: Vec<u8>, support: RangeSupport) -> Origin {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind origin");
        let port = listener.local_addr().expect("addr").port();
        let ranges = Arc::new(Mutex::new(Vec::new()));

        let sink = Arc::clone(&ranges);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let body = body.clone();
                let sink = Arc::clone(&sink);
                tokio::spawn(async move {
                    // A dropped connection just means the client gave up.
                    let _ = serve(socket, &body, support, sink).await;
                });
            }
        });
        Origin {
            port,
            ranges,
            handle,
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/artifact.bin", self.port)
    }

    fn observed_ranges(&self) -> Vec<Option<String>> {
        self.ranges.lock().expect("range log").clone()
    }

    fn request_count(&self) -> usize {
        self.ranges.lock().expect("range log").len()
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn serve(
    mut socket: tokio::net::TcpStream,
    body: &[u8],
    support: RangeSupport,
    ranges: Arc<Mutex<Vec<Option<String>>>>,
) -> std::io::Result<()> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if find(&buffer, b"\r\n\r\n").is_some() {
            break;
        }
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
    }

    let headers = String::from_utf8_lossy(&buffer);
    let range = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("range")
            .then(|| value.trim().to_string())
    });
    ranges.lock().expect("range log").push(range.clone());

    let start = range
        .as_deref()
        .and_then(|value| value.strip_prefix("bytes="))
        .and_then(|value| value.split('-').next())
        .and_then(|value| value.parse::<usize>().ok());

    let response = match (support, start) {
        (RangeSupport::Reject416, Some(_)) => {
            format!(
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes()
        }
        (RangeSupport::Honor, Some(start)) if start < body.len() => {
            let tail = &body[start..];
            let mut response = format!(
                "HTTP/1.1 206 Partial Content\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                start,
                body.len() - 1,
                body.len(),
                tail.len()
            )
            .into_bytes();
            response.extend_from_slice(tail);
            response
        }
        // `Ignore`, or a fresh request, or a range we can't satisfy: full body.
        _ => {
            let mut response = format!(
                "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            response.extend_from_slice(body);
            response
        }
    };

    socket.write_all(&response).await?;
    socket.flush().await?;
    let _ = socket.shutdown().await;
    Ok(())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn digest_of(bytes: &[u8]) -> Sha256Hex {
    let hash = sha2::Sha256::digest(bytes);
    Sha256Hex::new(hex::encode(hash)).expect("a valid digest")
}

fn body() -> Vec<u8> {
    (0..4096u32).map(|index| index as u8).collect()
}

/// `<dest>.part`, the file an interrupted transfer leaves behind.
fn part_of(dest: &std::path::Path) -> std::path::PathBuf {
    let mut name = dest.as_os_str().to_os_string();
    name.push(".part");
    std::path::PathBuf::from(name)
}

struct Fixture {
    _temp: tempfile::TempDir,
    dest: std::path::PathBuf,
    downloader: Downloader,
    body: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        Self {
            dest: temp.path().join("nested").join("artifact.bin"),
            _temp: temp,
            downloader: Downloader::new().expect("client"),
            body: body(),
        }
    }

    fn request(&self, origin: &Origin, sha256: Sha256Hex) -> DownloadRequest {
        DownloadRequest {
            url: origin.url(),
            dest: self.dest.clone(),
            sha256,
            progress: None,
        }
    }

    fn downloaded(&self) -> Vec<u8> {
        std::fs::read(&self.dest).expect("read the downloaded file")
    }
}

#[tokio::test]
async fn a_fresh_download_lands_verified_and_creates_its_parent_directory() {
    let fixture = Fixture::new();
    let origin = Origin::start(fixture.body.clone(), RangeSupport::Honor).await;

    fixture
        .downloader
        .fetch(&fixture.request(&origin, digest_of(&fixture.body)))
        .await
        .expect("download");

    assert_eq!(fixture.downloaded(), fixture.body);
    assert_eq!(origin.observed_ranges(), vec![None]);
    // The `.part` file is renamed into place, never left behind.
    assert!(!part_of(&fixture.dest).exists());
}

#[tokio::test]
async fn an_interrupted_transfer_resumes_from_where_it_stopped() {
    let fixture = Fixture::new();
    let origin = Origin::start(fixture.body.clone(), RangeSupport::Honor).await;

    // Simulate a transfer killed after 1000 bytes.
    std::fs::create_dir_all(fixture.dest.parent().expect("parent")).expect("mkdir");
    std::fs::write(part_of(&fixture.dest), &fixture.body[..1000]).expect("partial file");

    fixture
        .downloader
        .fetch(&fixture.request(&origin, digest_of(&fixture.body)))
        .await
        .expect("download");

    assert_eq!(fixture.downloaded(), fixture.body);
    // Only the tail was requested — that is the entire point.
    assert_eq!(
        origin.observed_ranges(),
        vec![Some("bytes=1000-".to_string())]
    );
}

#[tokio::test]
async fn an_origin_that_ignores_range_still_produces_a_correct_file() {
    let fixture = Fixture::new();
    let origin = Origin::start(fixture.body.clone(), RangeSupport::Ignore).await;

    std::fs::create_dir_all(fixture.dest.parent().expect("parent")).expect("mkdir");
    std::fs::write(part_of(&fixture.dest), &fixture.body[..1000]).expect("partial file");

    fixture
        .downloader
        .fetch(&fixture.request(&origin, digest_of(&fixture.body)))
        .await
        .expect("download");

    // The 200 response must truncate the partial file rather than append to it,
    // which would have produced 1000 bytes of duplicated prefix.
    assert_eq!(fixture.downloaded(), fixture.body);
    assert_eq!(origin.request_count(), 1);
}

#[tokio::test]
async fn a_partial_file_longer_than_the_object_restarts_the_download() {
    let fixture = Fixture::new();
    let origin = Origin::start(fixture.body.clone(), RangeSupport::Reject416).await;

    // Stale `.part` from a larger, older version of the artifact.
    std::fs::create_dir_all(fixture.dest.parent().expect("parent")).expect("mkdir");
    std::fs::write(part_of(&fixture.dest), vec![0xFFu8; 9000]).expect("partial file");

    fixture
        .downloader
        .fetch(&fixture.request(&origin, digest_of(&fixture.body)))
        .await
        .expect("download");

    assert_eq!(fixture.downloaded(), fixture.body);
    // A ranged request that got 416, then a clean one.
    assert_eq!(
        origin.observed_ranges(),
        vec![Some("bytes=9000-".to_string()), None]
    );
}

#[tokio::test]
async fn a_digest_mismatch_fails_and_deletes_the_poisoned_partial_file() {
    let fixture = Fixture::new();
    let origin = Origin::start(fixture.body.clone(), RangeSupport::Honor).await;

    let wrong = digest_of(b"different bytes entirely");
    let error = fixture
        .downloader
        .fetch(&fixture.request(&origin, wrong.clone()))
        .await
        .expect_err("digest mismatch");

    let Error::ChecksumMismatch {
        expected, actual, ..
    } = error
    else {
        panic!("expected a checksum mismatch");
    };
    assert_eq!(expected, wrong.as_str());
    assert_eq!(actual, digest_of(&fixture.body).as_str());

    // Left in place, the corrupt prefix would be resumed from forever.
    assert!(!part_of(&fixture.dest).exists());
    assert!(!fixture.dest.exists());
}

#[tokio::test]
async fn an_already_verified_file_is_not_downloaded_again() {
    let fixture = Fixture::new();
    let origin = Origin::start(fixture.body.clone(), RangeSupport::Honor).await;

    std::fs::create_dir_all(fixture.dest.parent().expect("parent")).expect("mkdir");
    std::fs::write(&fixture.dest, &fixture.body).expect("pre-existing file");

    fixture
        .downloader
        .fetch(&fixture.request(&origin, digest_of(&fixture.body)))
        .await
        .expect("download");

    assert_eq!(
        origin.request_count(),
        0,
        "no request should have been made"
    );
}

#[tokio::test]
async fn a_corrupt_existing_file_is_replaced() {
    let fixture = Fixture::new();
    let origin = Origin::start(fixture.body.clone(), RangeSupport::Honor).await;

    std::fs::create_dir_all(fixture.dest.parent().expect("parent")).expect("mkdir");
    std::fs::write(&fixture.dest, b"truncated garbage").expect("pre-existing file");

    fixture
        .downloader
        .fetch(&fixture.request(&origin, digest_of(&fixture.body)))
        .await
        .expect("download");

    assert_eq!(fixture.downloaded(), fixture.body);
    assert_eq!(origin.request_count(), 1);
}

#[tokio::test]
async fn progress_is_reported_and_ends_at_the_total() {
    let fixture = Fixture::new();
    let origin = Origin::start(fixture.body.clone(), RangeSupport::Honor).await;

    let seen: Arc<Mutex<Vec<Progress>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let mut request = fixture.request(&origin, digest_of(&fixture.body));
    request.progress = Some(Arc::new(move |progress| {
        sink.lock().expect("progress log").push(progress);
    }));

    fixture.downloader.fetch(&request).await.expect("download");

    let seen = seen.lock().expect("progress log").clone();
    let last = seen.last().expect("at least one progress report");
    assert_eq!(last.downloaded, fixture.body.len() as u64);
    assert_eq!(last.total, Some(fixture.body.len() as u64));
    assert_eq!(last.fraction(), Some(1.0));
}

#[tokio::test]
async fn resumed_progress_counts_the_bytes_already_on_disk() {
    let fixture = Fixture::new();
    let origin = Origin::start(fixture.body.clone(), RangeSupport::Honor).await;

    std::fs::create_dir_all(fixture.dest.parent().expect("parent")).expect("mkdir");
    std::fs::write(part_of(&fixture.dest), &fixture.body[..1000]).expect("partial file");

    let seen: Arc<Mutex<Vec<Progress>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let mut request = fixture.request(&origin, digest_of(&fixture.body));
    request.progress = Some(Arc::new(move |progress| {
        sink.lock().expect("progress log").push(progress);
    }));

    fixture.downloader.fetch(&request).await.expect("download");

    let seen = seen.lock().expect("progress log").clone();
    // The first report already accounts for the resumed bytes, so a progress bar
    // starts at 24% rather than jumping back to zero.
    assert_eq!(seen.first().expect("a report").downloaded, 1000);
    assert_eq!(
        seen.first().expect("a report").total,
        Some(fixture.body.len() as u64)
    );
    assert_eq!(
        seen.last().expect("a report").downloaded,
        fixture.body.len() as u64
    );
}

#[tokio::test]
async fn an_unreachable_origin_is_reported_as_a_transport_error() {
    let fixture = Fixture::new();
    // No origin: nothing is listening on this port.
    let error = fixture
        .downloader
        .fetch(&DownloadRequest {
            url: "http://127.0.0.1:1/artifact.bin".into(),
            dest: fixture.dest.clone(),
            sha256: digest_of(&fixture.body),
            progress: None,
        })
        .await
        .expect_err("connection refused");
    assert!(matches!(error, Error::Http { .. }), "{error:?}");
}
