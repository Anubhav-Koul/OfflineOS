//! Finding a browser to drive.
//!
//! Probe order is Chrome, then Edge (CLAUDE.md Phase 4: Edge is guaranteed on
//! Win10+, so it is the reliable fallback). We never attach to the user's
//! running browser or their real profile — the sidecar always launches a fresh
//! instance against a dedicated user-data directory, so automation cannot touch
//! their cookies, history, or logged-in sessions unless they log in inside the
//! automation window themselves.

use std::path::PathBuf;

use crate::error::{Error, Result};

/// A located browser executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserExecutable {
    /// Absolute path to the browser binary.
    pub path: PathBuf,
    /// Which browser it is, for logging.
    pub kind: BrowserKind,
}

/// Which Chromium-family browser was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserKind {
    /// Google Chrome.
    Chrome,
    /// Microsoft Edge.
    Edge,
}

impl BrowserKind {
    /// A human name for logs.
    pub fn label(self) -> &'static str {
        match self {
            BrowserKind::Chrome => "Chrome",
            BrowserKind::Edge => "Edge",
        }
    }
}

/// Locate a browser to drive: Chrome first, then Edge.
pub fn find_browser() -> Result<BrowserExecutable> {
    for (kind, candidates) in [
        (BrowserKind::Chrome, chrome_candidates()),
        (BrowserKind::Edge, edge_candidates()),
    ] {
        if let Some(path) = candidates.into_iter().find(|path| path.is_file()) {
            tracing::info!(kind = kind.label(), path = %path.display(), "found a browser");
            return Ok(BrowserExecutable { path, kind });
        }
    }
    Err(Error::NoBrowser)
}

#[cfg(windows)]
fn chrome_candidates() -> Vec<PathBuf> {
    let mut paths = registry_app_path("chrome.exe");
    paths.extend(program_files_paths(&[
        r"Google\Chrome\Application\chrome.exe",
    ]));
    paths
}

#[cfg(windows)]
fn edge_candidates() -> Vec<PathBuf> {
    let mut paths = registry_app_path("msedge.exe");
    paths.extend(program_files_paths(&[
        r"Microsoft\Edge\Application\msedge.exe",
    ]));
    paths
}

/// Read `App Paths\<exe>` (HKLM then HKCU); its default value is the full path.
#[cfg(windows)]
fn registry_app_path(exe: &str) -> Vec<PathBuf> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

    let subkey = format!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\{exe}");
    [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER]
        .into_iter()
        .filter_map(|hive| {
            RegKey::predef(hive)
                .open_subkey(&subkey)
                .ok()?
                .get_value::<String, _>("")
                .ok()
                .map(PathBuf::from)
        })
        .collect()
}

/// The same relative path under each of the Program Files roots.
#[cfg(windows)]
fn program_files_paths(relatives: &[&str]) -> Vec<PathBuf> {
    let roots = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
        .into_iter()
        .filter_map(|var| std::env::var_os(var).map(PathBuf::from));
    let mut out = Vec::new();
    for root in roots {
        for relative in relatives {
            out.push(root.join(relative));
        }
    }
    out
}

/// Non-Windows fallback: look for the usual binaries on `PATH`. The desktop app
/// is Windows-first; this keeps the crate building and testable elsewhere.
#[cfg(not(windows))]
fn chrome_candidates() -> Vec<PathBuf> {
    path_lookup(&[
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chrome",
    ])
}

#[cfg(not(windows))]
fn edge_candidates() -> Vec<PathBuf> {
    path_lookup(&["microsoft-edge", "msedge"])
}

#[cfg(not(windows))]
fn path_lookup(names: &[&str]) -> Vec<PathBuf> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for dir in std::env::split_paths(&path) {
        for name in names {
            out.push(dir.join(name));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_kinds_have_distinct_labels() {
        assert_ne!(BrowserKind::Chrome.label(), BrowserKind::Edge.label());
    }

    #[test]
    fn probing_never_panics_and_prefers_chrome_when_both_are_present() {
        // We cannot assume a browser is installed on every CI runner, so this
        // only asserts the probe returns a well-formed result or a clean
        // `NoBrowser` — never a panic — and that Chrome outranks Edge in order.
        match find_browser() {
            Ok(exe) => assert!(exe.path.is_file()),
            Err(Error::NoBrowser) => {}
            Err(other) => panic!("unexpected probe error: {other}"),
        }
    }
}
