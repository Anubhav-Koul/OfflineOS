//! Remembering where the widget was, per monitor arrangement.
//!
//! An always-on-top widget that reappears under a monitor the user unplugged is
//! useless — and on Windows it cannot be dragged back, because its title bar is
//! offscreen along with the rest of it. Docking a laptop, unplugging a monitor,
//! or changing scaling all move the desktop's coordinate space out from under a
//! saved `(x, y)`.
//!
//! So positions are keyed by a **hash of the monitor arrangement**, not stored
//! as a single pair. Dock the laptop and the widget returns to where it sat on
//! the external display; undock and it returns to where it sat on the built-in
//! one. A layout that has never been seen gets no position, and the caller
//! centres the widget instead. A saved position that is no longer on any monitor
//! is discarded on read, which covers a monitor that kept its identity but
//! changed resolution.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::error::{Error, Result};

/// How much of the widget must remain on a monitor for a saved position to be
/// considered usable. Enough to grab with a mouse.
const MIN_VISIBLE_PIXELS: i32 = 48;

/// One monitor, as the windowing layer reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorInfo {
    /// Platform monitor name. May be absent; the geometry still identifies it.
    pub name: Option<String>,
    /// Top-left in the virtual desktop's coordinate space.
    pub x: i32,
    /// Top-left in the virtual desktop's coordinate space.
    pub y: i32,
    /// Physical width in pixels.
    pub width: u32,
    /// Physical height in pixels.
    pub height: u32,
    /// DPI scale factor. A scaling change moves every window.
    pub scale: f64,
}

impl MonitorInfo {
    /// Whether `position` lies far enough inside this monitor to be grabbable.
    fn contains_usable(&self, position: WindowPosition) -> bool {
        let right = self.x.saturating_add(self.width as i32);
        let bottom = self.y.saturating_add(self.height as i32);
        position.x >= self.x
            && position.y >= self.y
            && position.x <= right.saturating_sub(MIN_VISIBLE_PIXELS)
            && position.y <= bottom.saturating_sub(MIN_VISIBLE_PIXELS)
    }
}

/// A stable fingerprint of one monitor arrangement.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LayoutHash(String);

impl LayoutHash {
    /// Fingerprint the arrangement.
    ///
    /// Independent of enumeration order — the OS is free to list monitors in any
    /// order between boots, and a reshuffle must not look like a new layout.
    /// Depends on each monitor's position, size, and scale, because a change to
    /// any of those invalidates a saved coordinate.
    pub fn of(monitors: &[MonitorInfo]) -> Self {
        let mut fingerprints: Vec<String> = monitors
            .iter()
            .map(|monitor| {
                format!(
                    "{}:{}:{}:{}:{}:{:016x}",
                    monitor.name.as_deref().unwrap_or(""),
                    monitor.x,
                    monitor.y,
                    monitor.width,
                    monitor.height,
                    // Hash the bit pattern: 1.25 and 1.2500000001 are different
                    // layouts, and `f64` has no `Hash`.
                    monitor.scale.to_bits()
                )
            })
            .collect();
        fingerprints.sort();

        let mut hasher = Sha256::new();
        for fingerprint in &fingerprints {
            hasher.update(fingerprint.as_bytes());
            hasher.update([0]); // length-delimit so "ab"+"c" != "a"+"bc"
        }
        Self(hex::encode(&hasher.finalize()[..16]))
    }

    /// The fingerprint as text, which is the key in the persisted map.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The widget's top-left corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowPosition {
    /// Virtual-desktop x.
    pub x: i32,
    /// Virtual-desktop y.
    pub y: i32,
}

/// Saved positions, one per monitor arrangement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowState {
    /// Keyed by [`LayoutHash::as_str`].
    #[serde(default)]
    positions: BTreeMap<String, WindowPosition>,
}

impl WindowState {
    /// The saved position for this arrangement, if it is still on a monitor.
    ///
    /// Returns `None` when the arrangement is new, or when the saved point has
    /// drifted offscreen (a monitor kept its name but changed resolution). The
    /// caller should centre the widget.
    pub fn position_for(
        &self,
        layout: &LayoutHash,
        monitors: &[MonitorInfo],
    ) -> Option<WindowPosition> {
        let position = *self.positions.get(layout.as_str())?;
        monitors
            .iter()
            .any(|monitor| monitor.contains_usable(position))
            .then_some(position)
    }

    /// Remember where the widget is for this arrangement.
    pub fn remember(&mut self, layout: &LayoutHash, position: WindowPosition) {
        self.positions.insert(layout.as_str().to_string(), position);
    }

    /// Forget every saved position. The tray's "reset position" action.
    pub fn forget_all(&mut self) {
        self.positions.clear();
    }
}

/// Reads and writes [`WindowState`] as JSON.
#[derive(Debug, Clone)]
pub struct WindowStateStore {
    path: PathBuf,
}

impl WindowStateStore {
    /// Store the state at `path`.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The default location, `%LOCALAPPDATA%\IronClaw Desktop\window-state.json`.
    pub fn default_path() -> Result<PathBuf> {
        let base = dirs::data_local_dir().ok_or_else(|| {
            Error::io(
                "locating the local application data directory",
                std::io::Error::from(std::io::ErrorKind::NotFound),
            )
        })?;
        Ok(base.join("IronClaw Desktop").join("window-state.json"))
    }

    /// Where the state lives.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the saved state.
    ///
    /// A missing file is an empty state — the first launch. A *corrupt* file is
    /// also an empty state, and is reported: window position is not worth
    /// refusing to start over, but silently discarding it would hide a bug that
    /// writes bad JSON.
    pub fn load(&self) -> Result<WindowState> {
        let contents = match std::fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(WindowState::default());
            }
            Err(source) => {
                return Err(Error::io(
                    format!("reading {}", self.path.display()),
                    source,
                ));
            }
        };
        match serde_json::from_str(&contents) {
            Ok(state) => Ok(state),
            Err(error) => {
                tracing::warn!(
                    path = %self.path.display(),
                    %error,
                    "discarding an unreadable window-state file"
                );
                Ok(WindowState::default())
            }
        }
    }

    /// Write the state, atomically.
    ///
    /// A crash mid-write would otherwise leave truncated JSON, which the next
    /// launch would discard — losing every remembered position, not just the one
    /// being saved.
    pub fn save(&self, state: &WindowState) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|source| Error::io(format!("creating {}", parent.display()), source))?;
        }
        let json = serde_json::to_string_pretty(state).map_err(|source| Error::Json {
            context: "serializing the window state".into(),
            source,
        })?;

        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, json)
            .map_err(|source| Error::io(format!("writing {}", temporary.display()), source))?;
        std::fs::rename(&temporary, &self.path).map_err(|source| {
            Error::io(format!("moving {} into place", temporary.display()), source)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(name: &str, x: i32, y: i32) -> MonitorInfo {
        MonitorInfo {
            name: Some(name.into()),
            x,
            y,
            width: 1920,
            height: 1080,
            scale: 1.0,
        }
    }

    #[test]
    fn the_layout_hash_ignores_enumeration_order() {
        let laptop = monitor("laptop", 0, 0);
        let external = monitor("external", 1920, 0);
        // The OS may list monitors in either order between boots.
        assert_eq!(
            LayoutHash::of(&[laptop.clone(), external.clone()]),
            LayoutHash::of(&[external, laptop])
        );
    }

    #[test]
    fn unplugging_a_monitor_changes_the_layout() {
        let laptop = monitor("laptop", 0, 0);
        let external = monitor("external", 1920, 0);
        assert_ne!(
            LayoutHash::of(&[laptop.clone(), external]),
            LayoutHash::of(std::slice::from_ref(&laptop))
        );
    }

    #[test]
    fn moving_or_rescaling_a_monitor_changes_the_layout() {
        let base = monitor("laptop", 0, 0);
        let baseline = LayoutHash::of(std::slice::from_ref(&base));

        let moved = MonitorInfo {
            x: 100,
            ..base.clone()
        };
        assert_ne!(baseline, LayoutHash::of(&[moved]));

        let resized = MonitorInfo {
            width: 2560,
            ..base.clone()
        };
        assert_ne!(baseline, LayoutHash::of(&[resized]));

        // A scaling change moves every window, so it is a different layout.
        let rescaled = MonitorInfo {
            scale: 1.25,
            ..base
        };
        assert_ne!(baseline, LayoutHash::of(&[rescaled]));
    }

    #[test]
    fn the_hash_is_length_delimited_so_adjacent_fields_cannot_be_confused() {
        // Without a delimiter, "ab" + "c" would hash the same as "a" + "bc".
        let a = MonitorInfo {
            name: Some("ab".into()),
            ..monitor("x", 0, 0)
        };
        let b = MonitorInfo {
            name: Some("a".into()),
            ..monitor("x", 0, 0)
        };
        assert_ne!(LayoutHash::of(&[a]), LayoutHash::of(&[b]));
    }

    #[test]
    fn a_position_is_returned_only_for_the_layout_it_was_saved_on() {
        let docked = [monitor("laptop", 0, 0), monitor("external", 1920, 0)];
        let undocked = [monitor("laptop", 0, 0)];
        let docked_layout = LayoutHash::of(&docked);
        let undocked_layout = LayoutHash::of(&undocked);

        let mut state = WindowState::default();
        // Sitting on the external monitor while docked.
        state.remember(&docked_layout, WindowPosition { x: 2400, y: 300 });

        assert_eq!(
            state.position_for(&docked_layout, &docked),
            Some(WindowPosition { x: 2400, y: 300 })
        );
        // Undocked, that point is on no monitor, and the layout is different
        // anyway. The widget must be centred, not restored offscreen.
        assert_eq!(state.position_for(&undocked_layout, &undocked), None);
    }

    #[test]
    fn a_saved_position_that_drifted_offscreen_is_discarded() {
        let monitors = [monitor("laptop", 0, 0)];
        let layout = LayoutHash::of(&monitors);
        let mut state = WindowState::default();

        // Right edge: the widget would be a sliver, ungrabbable.
        state.remember(&layout, WindowPosition { x: 1900, y: 500 });
        assert_eq!(state.position_for(&layout, &monitors), None);

        // Negative coordinates are legal on a multi-monitor desktop, but not on
        // this one.
        state.remember(&layout, WindowPosition { x: -500, y: 100 });
        assert_eq!(state.position_for(&layout, &monitors), None);

        state.remember(&layout, WindowPosition { x: 100, y: 100 });
        assert!(state.position_for(&layout, &monitors).is_some());
    }

    #[test]
    fn a_position_on_a_secondary_monitor_with_negative_origin_is_kept() {
        // A monitor placed to the left of the primary has negative coordinates.
        let monitors = [monitor("primary", 0, 0), monitor("left", -1920, 0)];
        let layout = LayoutHash::of(&monitors);
        let mut state = WindowState::default();
        state.remember(&layout, WindowPosition { x: -1800, y: 200 });
        assert_eq!(
            state.position_for(&layout, &monitors),
            Some(WindowPosition { x: -1800, y: 200 })
        );
    }

    #[test]
    fn forget_all_clears_every_layout() {
        let monitors = [monitor("laptop", 0, 0)];
        let layout = LayoutHash::of(&monitors);
        let mut state = WindowState::default();
        state.remember(&layout, WindowPosition { x: 10, y: 10 });
        state.forget_all();
        assert_eq!(state.position_for(&layout, &monitors), None);
    }

    #[test]
    fn state_round_trips_through_the_store() {
        let temp = tempfile::tempdir().expect("tempdir");
        // A nested path: the store must create the directory.
        let store = WindowStateStore::at(temp.path().join("nested").join("window-state.json"));
        assert_eq!(store.load().expect("empty"), WindowState::default());

        let monitors = [monitor("laptop", 0, 0)];
        let layout = LayoutHash::of(&monitors);
        let mut state = WindowState::default();
        state.remember(&layout, WindowPosition { x: 42, y: 7 });
        store.save(&state).expect("save");

        assert_eq!(store.load().expect("load"), state);
        // No temp file is left behind.
        assert!(!store.path().with_extension("json.tmp").exists());
    }

    #[test]
    fn a_corrupt_state_file_is_discarded_rather_than_failing_the_launch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("window-state.json");
        std::fs::write(&path, "{ this is not json").expect("write");

        let store = WindowStateStore::at(&path);
        assert_eq!(store.load().expect("load"), WindowState::default());
    }
}
