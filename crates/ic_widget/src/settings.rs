//! User settings that outlive a launch.
//!
//! The active LLM provider and the character choice. Persisted as JSON beside
//! the window state, with the same discipline: a missing file is first-run defaults, a
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
        /// An endpoint override. Required for `openai_compatible` (which has no
        /// endpoint of its own) and honoured for anyone pointing a provider at a
        /// proxy or a regional URL.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
    },
}

impl Default for ProviderSelection {
    /// A fresh install has no cloud keys, so the local model is the only thing
    /// that can work offline out of the box.
    fn default() -> Self {
        ProviderSelection::Local
    }
}

/// The cloud provider the *local* model falls back to when it cannot answer.
///
/// Deliberately not a [`ProviderSelection`]: a fallback is only meaningful
/// alongside a local model, it is reached through the `ic_llama` proxy rather
/// than through `LLM_BACKEND`, and it must be a provider that can be spoken to
/// in the OpenAI shape (`providers::Provider::can_fail_over`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FallbackProvider {
    /// The provider id, e.g. `anthropic`.
    pub id: String,
    /// A model override, or `None` for the provider's catalog default.
    #[serde(default)]
    pub model: Option<String>,
}

/// How a reply reaches the user.
///
/// The user may want to *read* the answer, *hear* it, or both. Before this,
/// speech was unconditional whenever voice was enabled, so a user who only wanted
/// a wake word had no way to stop the character talking back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyMode {
    /// Show the speech bubble; stay silent.
    Read,
    /// Speak the reply; no bubble.
    Hear,
    /// Both — the bubble is shown and the reply is spoken, with lip sync.
    Both,
}

impl Default for ReplyMode {
    /// Reading always works. Speaking needs voice enabled *and* the TTS models
    /// downloaded, so it cannot be the default a fresh install lands on.
    fn default() -> Self {
        ReplyMode::Read
    }
}

impl ReplyMode {
    /// Whether a reply is shown in the speech bubble.
    pub fn shows_bubble(self) -> bool {
        matches!(self, ReplyMode::Read | ReplyMode::Both)
    }

    /// Whether a reply is spoken.
    pub fn speaks(self) -> bool {
        matches!(self, ReplyMode::Hear | ReplyMode::Both)
    }
}

/// A window of the local day in which the character never speaks first.
///
/// Half-open `[start, end)` in **local** hours, and it may wrap past midnight
/// (`22 → 8` is a night). `start == end` is an empty window, not a full day — a
/// quiet period that silenced everything forever would be indistinguishable from
/// the feature being broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuietHours {
    /// First quiet hour, local, `0..=23`.
    pub start_hour: u32,
    /// First hour that is loud again, local, `0..=23`.
    pub end_hour: u32,
}

impl QuietHours {
    /// Whether `hour` (local, `0..=23`) falls inside the window.
    pub fn contains(&self, hour: u32) -> bool {
        let (start, end) = (self.start_hour % 24, self.end_hour % 24);
        if start == end {
            return false;
        }
        if start < end {
            (start..end).contains(&hour)
        } else {
            // Wraps midnight: 22..24 or 0..8.
            hour >= start || hour < end
        }
    }
}

/// How the ambient companion is allowed to interrupt (Phase 7a).
///
/// Everything here is a *guardrail*, not a feature switch — the master switch is
/// [`Settings::ambient_enabled`], which is off until the user asks for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AmbientSettings {
    /// Hard cap on unsolicited surfacings per rolling hour.
    #[serde(default = "default_max_per_hour")]
    pub max_per_hour: u32,
    /// When the character stays quiet, or `None` for no quiet window.
    #[serde(default = "default_quiet_hours")]
    pub quiet_hours: Option<QuietHours>,
    /// The ambient thread — the conversation the *app* starts, not the user.
    ///
    /// Persisted so it survives a restart (threads outlive the gateway process).
    /// `None` until ambient mode is first switched on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

fn default_max_per_hour() -> u32 {
    2
}

fn default_quiet_hours() -> Option<QuietHours> {
    Some(QuietHours {
        start_hour: 22,
        end_hour: 8,
    })
}

impl Default for AmbientSettings {
    fn default() -> Self {
        Self {
            max_per_hour: default_max_per_hour(),
            quiet_hours: default_quiet_hours(),
            thread_id: None,
        }
    }
}

/// Everything persisted between launches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// What the user is called. Empty until the setup wizard asks. Goes into the
    /// agent's system prompt, so it is a fact the model actually knows rather than
    /// a label the UI paints on.
    #[serde(default)]
    pub user_name: String,
    /// What the assistant is called — its own name, and (once wake-word models are
    /// recorded) the phrase that wakes it.
    #[serde(default)]
    pub assistant_name: String,
    /// Whether a reply is read, heard, or both.
    #[serde(default)]
    pub reply_mode: ReplyMode,
    /// The global summon / push-to-talk hotkey. `None` uses the default
    /// (Ctrl+Alt+Space). Rebindable because the default is commonly taken — when it
    /// is, registration fails and push-to-talk silently never fires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summon_hotkey: Option<String>,
    /// The LLM the gateway starts with.
    #[serde(default)]
    pub active_provider: ProviderSelection,
    /// Which installed GGUF the local model runs, by id. `None` means "the first
    /// usable one", which is what the app did before a model could be pinned.
    /// A pinned model that is missing or suspect falls back to that same rule
    /// rather than refusing to start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// The cloud provider a local model falls back to when it cannot answer.
    ///
    /// This is **not** a second `LLM_BACKEND` — the gateway only ever knows
    /// about one provider. The `ic_llama` proxy owns the retry, so the cloud key
    /// never enters the gateway's environment at all. See
    /// `docs/desktop/llm-provider-selection.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_fallback: Option<FallbackProvider>,
    /// Which character asset folder the widget renders, or `None` for the
    /// default. A character is data (Phase 3): swapping folders needs no code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub character: Option<CharacterId>,
    /// Whether the voice pipeline runs. Off by default: enabling it downloads the
    /// speech models (~210 MB) on first use, so it is an explicit opt-in (Phase 5).
    #[serde(default)]
    pub voice_enabled: bool,
    /// Whether the microphone starts muted when voice is enabled.
    #[serde(default)]
    pub voice_muted: bool,
    /// Which input device to record from. `None` follows the OS default.
    ///
    /// The default is frequently wrong and *silently* so: a paired Bluetooth
    /// headset takes the default input slot and its HFP endpoint delivers a steady
    /// stream of near-silence — live enough to pass a "does it produce samples?"
    /// probe, deaf enough that nothing is ever heard. So the microphone is a choice
    /// the user can make, and it has to survive restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_device: Option<String>,
    /// Whether the first-run setup wizard has been completed. `false` on a fresh
    /// install shows the wizard (pick a model / provider, optionally enable voice).
    #[serde(default)]
    pub setup_complete: bool,
    /// Whether the character may speak first (Phase 7). **Off by default**, and it
    /// is the only thing that lets the agent run a turn nobody asked for: it also
    /// switches on the gateway's trigger poller, which is what makes a scheduled
    /// automation actually fire. Off means no unprompted run happens at all.
    #[serde(default)]
    pub ambient_enabled: bool,
    /// The guardrails that bound ambient mode when it *is* on.
    #[serde(default)]
    pub ambient: AmbientSettings,
    /// Whether a completed task earns a reflection turn that may draft a skill
    /// (Phase 7b). **Off by default**, and it only matters while
    /// [`Settings::ambient_enabled`] is also on — reflection runs on the ambient
    /// thread and every draft it surfaces rides the ambient guardrails.
    #[serde(default)]
    pub reflection_enabled: bool,
    /// Conversations the user has hidden from the Chats list (Phase 8a).
    ///
    /// **A local archive, not a delete.** `ironclaw-reborn serve` exposes no
    /// route that removes a thread (verified — every spelling 404s), and reaching
    /// into its libSQL to delete rows would both couple us to internals and break
    /// the never-delete-LLM-data invariant. So the thread lives on in the gateway
    /// and this list is the widget's own "don't show me that one" — which is
    /// exactly what the button says.
    #[serde(default)]
    pub hidden_threads: Vec<String>,
    /// Event-driven proactivity (Phase 7d): what the app may watch, and the
    /// user's "when X happens, ask the agent to Y" rules. Every signal kind is
    /// individually opt-in and **off by default**, and none of it runs unless
    /// [`Settings::ambient_enabled`] is also on.
    #[serde(default)]
    pub watchers: WatcherSettings,
    /// Connector OAuth (Phase 8b.1). Holds the one thing about the OAuth callback
    /// that is not secret and not derivable: the fixed loopback port the redirect
    /// lands on. The Google OAuth *client* itself lives in the credential store.
    #[serde(default)]
    pub google_oauth: GoogleOAuthSettings,
}

/// Connector OAuth configuration (Phase 8b.1).
///
/// Google matches a registered redirect URI byte-for-byte, but the widget takes
/// a fresh OS-assigned port for `serve` at every launch — so OAuth needs a
/// *stable* callback the user registers with Google once. The widget owns a
/// small loopback listener on this fixed port and proxies the redirect into
/// `serve`'s dynamic callback route. Configurable because 51789 may be taken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoogleOAuthSettings {
    /// The fixed loopback port the OAuth redirect lands on. Default 51789 — an
    /// uncommon port, chosen to rarely collide.
    #[serde(default = "default_oauth_callback_port")]
    pub callback_port: u16,
}

impl Default for GoogleOAuthSettings {
    fn default() -> Self {
        Self {
            callback_port: default_oauth_callback_port(),
        }
    }
}

/// The default fixed loopback port for connector OAuth redirects.
fn default_oauth_callback_port() -> u16 {
    51789
}

/// The ambient watchers' configuration (Phase 7d).
///
/// Three signal kinds, each with its own switch — a user who wants folder
/// watching has not agreed to window-title sampling. No screen content is ever
/// captured, no audio persists, and nothing leaves the machine: a signal is a
/// window *title*, a file *path*, or the local clock.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatcherSettings {
    /// May the app sample the foreground window's title?
    #[serde(default)]
    pub foreground_enabled: bool,
    /// May the app watch the folders named by rules?
    #[serde(default)]
    pub folders_enabled: bool,
    /// May time-of-day rules fire?
    #[serde(default)]
    pub time_enabled: bool,
    /// The user's rules. A rule whose signal kind is switched off never fires.
    #[serde(default)]
    pub rules: Vec<WatchRule>,
}

/// One user-defined "when X happens, ask the agent to Y" rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchRule {
    /// Stable id, minted by whoever creates the rule.
    pub id: String,
    /// A rule can be kept but paused.
    #[serde(default = "default_rule_enabled")]
    pub enabled: bool,
    /// The X: what fires it.
    pub trigger: WatchTrigger,
    /// The Y: the prompt materialized on a fresh thread when it fires.
    pub prompt: String,
}

fn default_rule_enabled() -> bool {
    true
}

/// What a [`WatchRule`] watches for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WatchTrigger {
    /// The foreground window's title starts containing `title_contains`
    /// (case-insensitive, fires on the transition, not the state).
    ForegroundApp {
        /// The substring to look for.
        title_contains: String,
    },
    /// Anything under `path` changes.
    FolderChanged {
        /// The folder being watched, recursively.
        path: String,
    },
    /// The local clock reaches `hour:minute`, once per day.
    TimeOfDay {
        /// Local hour, 0–23.
        hour: u32,
        /// Local minute, 0–59.
        minute: u32,
    },
}

impl WatchTrigger {
    /// A one-line human description, used as the suggestion headline's stem.
    pub fn describe(&self) -> String {
        match self {
            WatchTrigger::ForegroundApp { title_contains } => {
                format!("a window with \u{201c}{title_contains}\u{201d} came to front")
            }
            WatchTrigger::FolderChanged { path } => format!("{path} changed"),
            WatchTrigger::TimeOfDay { hour, minute } => {
                format!("it is {hour:02}:{minute:02}")
            }
        }
    }
}

/// A character asset folder's name, e.g. `hiyori`.
///
/// It becomes a URL path segment (`/characters/<id>/character.json`), so the
/// alphabet is restricted at construction — see `.claude/rules/types.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct CharacterId(String);

impl CharacterId {
    fn validate(s: &str) -> std::result::Result<(), String> {
        if s.is_empty() || s.len() > 64 {
            return Err("a character id must be 1–64 characters".into());
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            return Err("a character id may only use a–z, 0–9, '-' and '_'".into());
        }
        Ok(())
    }

    /// Validate and wrap a raw id.
    pub fn new(raw: impl Into<String>) -> std::result::Result<Self, String> {
        let s = raw.into();
        Self::validate(&s)?;
        Ok(Self(s))
    }

    /// The id as a path segment.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CharacterId {
    type Error = String;
    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        Self::validate(&value)?;
        Ok(Self(value))
    }
}

impl std::fmt::Display for CharacterId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
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
        // Strip a UTF-8 byte-order mark. `serde_json` rejects one outright
        // ("expected value at line 1 column 1"), and plenty of Windows tools
        // write it without asking — PowerShell 5.1's `Set-Content -Encoding utf8`
        // does, and Notepad used to. A BOM'd file would otherwise read as
        // *corrupt*, silently resetting every setting the user has.
        let contents = contents.strip_prefix('\u{feff}').unwrap_or(&contents);

        match serde_json::from_str(contents) {
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

    /// A settings file with a UTF-8 BOM must still load.
    ///
    /// Not hypothetical: PowerShell 5.1's `Set-Content -Encoding utf8` writes
    /// one, and `serde_json` rejects it at column 1 — so the file read as
    /// *corrupt* and every setting silently reset to defaults. A user who edits
    /// their settings in the wrong editor should not lose their assistant's name.
    #[test]
    fn a_settings_file_with_a_byte_order_mark_still_loads() {
        let (_dir, store) = store();
        let saved = Settings {
            assistant_name: "Nova".to_string(),
            ambient_enabled: true,
            ..Settings::default()
        };
        store.save(&saved).expect("save");

        // Re-write it exactly as a BOM-writing editor would.
        let json = std::fs::read_to_string(store.path()).expect("read back");
        std::fs::write(store.path(), format!("\u{feff}{json}")).expect("write with a BOM");

        let loaded = store.load().expect("load");
        assert_eq!(
            loaded, saved,
            "a byte-order mark must not silently reset the user's settings"
        );
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
                base_url: None,
            },
            ..Default::default()
        };
        store.save(&settings).expect("save");
        assert_eq!(store.load().expect("load"), settings);
    }

    /// A selection written by an older build has no `base_url` field at all. It
    /// must still load — a settings file is a contract with the user's past self.
    #[test]
    fn a_cloud_selection_from_before_custom_endpoints_still_loads() {
        let legacy = r#"{"kind":"cloud","id":"openai","model":"gpt-5-mini"}"#;
        let selection: ProviderSelection = serde_json::from_str(legacy).expect("decode");
        assert_eq!(
            selection,
            ProviderSelection::Cloud {
                id: "openai".into(),
                model: Some("gpt-5-mini".into()),
                base_url: None,
            }
        );
    }

    #[test]
    fn a_character_choice_round_trips_and_an_absent_one_stays_default() {
        let (_dir, store) = store();
        assert_eq!(store.load().expect("load").character, None);

        let settings = Settings {
            character: Some(CharacterId::new("ren").expect("valid id")),
            ..Default::default()
        };
        store.save(&settings).expect("save");
        assert_eq!(store.load().expect("load"), settings);
    }

    #[test]
    fn a_character_id_is_a_safe_path_segment_or_it_is_rejected() {
        for good in ["hiyori", "ren", "my-model_2"] {
            assert!(CharacterId::new(good).is_ok(), "{good}");
        }
        for bad in ["", "Ren", "a/b", "..", "a b", &"x".repeat(65)] {
            assert!(CharacterId::new(bad).is_err(), "{bad:?}");
        }
        // Wire validation matches construction: a hostile settings file cannot
        // smuggle a path traversal through serde.
        assert!(serde_json::from_str::<CharacterId>(r#""../evil""#).is_err());
    }

    #[test]
    fn the_selection_tag_is_snake_case_on_the_wire() {
        // The frontend switches on `kind`, so the wire spelling is a contract.
        let json = serde_json::to_string(&ProviderSelection::Cloud {
            id: "openai".into(),
            model: None,
            base_url: None,
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
