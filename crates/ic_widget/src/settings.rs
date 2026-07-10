//! User settings that outlive a launch.
//!
//! Just the active LLM provider today. Persisted as JSON beside the window
//! state, with the same discipline: a missing file is first-run defaults, a
//! corrupt file is reported and replaced rather than failing the launch, and
//! writes are atomic so a crash mid-write cannot truncate it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Which LLM the gateway is pointed at.
///
/// Exactly one is active: `LLM_BACKEND` holds a single value, so the local
/// sidecar and a cloud provider are mutually exclusive. See
/// `docs/desktop/llm-provider-selection.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderSelection {
    /// The bundled llama.cpp sidecar, using whichever model is installed.
    Local,
    /// A provider from the catalog (`providers.json`), keyed by its
    /// `LLM_BACKEND` id, optionally pinning a model over the catalog default.
    Cloud {
        /// The provider id, e.g. `anthropic`.
        id: String,
        /// A model override, or `None` for the provider's default.
        #[serde(default)]
        model: Option<String>,
    },
}

impl Default for ProviderSelection {
    /// A fresh install has no cloud keys, so the local model is the only thing
    /// that can work offline out of the box.
    fn default() -> Self {
        ProviderSelection::Local
    }
}

/// Everything persisted between launches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// The LLM the gateway starts with.
    #[serde(default)]
    pub active_provider: ProviderSelection,
}

/// Reads and writes [`Settings`] as JSON.
#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    /// Store the settings at `path`.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The default location, `%LOCALAPPDATA%\IronClaw Desktop\settings.json`.
    pub fn default_path() -> Result<PathBuf> {
        let base = dirs::data_local_dir().ok_or_else(|| {
            Error::io(
                "locating the local application data directory",
                std::io::Error::from(std::io::ErrorKind::NotFound),
            )
        })?;
        Ok(base.join("IronClaw Desktop").join("settings.json"))
    }

    /// Where the settings live.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the saved settings.
    ///
    /// A missing file is the defaults — the first launch. A *corrupt* file is
    /// also the defaults, and is reported: a provider choice is not worth
    /// refusing to start over, but silently discarding it would hide a bug that
    /// writes bad JSON.
    pub fn load(&self) -> Result<Settings> {
        let contents = match std::fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Settings::default());
            }
            Err(source) => {
                return Err(Error::io(
                    format!("reading {}", self.path.display()),
                    source,
                ));
            }
        };
        match serde_json::from_str(&contents) {
            Ok(settings) => Ok(settings),
            Err(error) => {
                tracing::warn!(
                    path = %self.path.display(),
                    %error,
                    "discarding an unreadable settings file"
                );
                Ok(Settings::default())
            }
        }
    }

    /// Write the settings, atomically.
    pub fn save(&self, settings: &Settings) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|source| Error::io(format!("creating {}", parent.display()), source))?;
        }

        let json = serde_json::to_string_pretty(settings).map_err(|source| Error::Json {
            context: "serializing the settings".into(),
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

    fn store() -> (tempfile::TempDir, SettingsStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SettingsStore::at(dir.path().join("settings.json"));
        (dir, store)
    }

    #[test]
    fn a_first_launch_has_no_file_and_defaults_to_the_local_model() {
        let (_dir, store) = store();
        let settings = store.load().expect("a missing file is the defaults");
        assert_eq!(settings.active_provider, ProviderSelection::Local);
    }

    #[test]
    fn a_cloud_selection_round_trips_through_the_store() {
        let (_dir, store) = store();
        let settings = Settings {
            active_provider: ProviderSelection::Cloud {
                id: "anthropic".into(),
                model: Some("claude-opus-4-8".into()),
            },
        };
        store.save(&settings).expect("save");
        assert_eq!(store.load().expect("load"), settings);
    }

    #[test]
    fn the_selection_tag_is_snake_case_on_the_wire() {
        // The frontend switches on `kind`, so the wire spelling is a contract.
        let json = serde_json::to_string(&ProviderSelection::Cloud {
            id: "openai".into(),
            model: None,
        })
        .expect("serialize");
        assert!(json.contains(r#""kind":"cloud""#), "got {json}");
        assert!(json.contains(r#""id":"openai""#), "got {json}");

        let local = serde_json::to_string(&ProviderSelection::Local).expect("serialize");
        assert_eq!(local, r#"{"kind":"local"}"#);
    }

    #[test]
    fn a_corrupt_file_is_discarded_rather_than_failing_the_launch() {
        let (_dir, store) = store();
        std::fs::write(store.path(), "{ not json").expect("write garbage");
        assert_eq!(
            store.load().expect("a corrupt file must not fail the load"),
            Settings::default()
        );
    }
}
