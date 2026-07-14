import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

/**
 * The typed edge of the Rust bridge.
 *
 * Every field name here matches a `serde` field on a command return type in
 * `crates/ic_widget/src/main.rs`. Nothing else in the UI calls `invoke` or
 * `listen`, so a change to a command's shape breaks in exactly one file.
 */

export type GatewayState =
  | { state: "starting" }
  | { state: "ready" }
  | { state: "restarting"; attempt: number; backoff_ms: number }
  | { state: "unhealthy"; reason: string }
  | { state: "stopped" };

/**
 * A run's lifecycle position. Mirrors `ic_widget::gateway_client::RunPhase`.
 *
 * `blocked_*` means the agent is waiting on the user. Only `completed`,
 * `cancelled`, `failed`, and `killed` are terminal.
 */
export type RunPhase =
  | "queued"
  | "running"
  | "cancel_requested"
  | "blocked_approval"
  | "blocked_auth"
  | "blocked_resource"
  | "blocked_dependent_run"
  | "recovery_required"
  | "completed"
  | "cancelled"
  | "failed"
  | "killed"
  | "other";

export const TERMINAL_PHASES: ReadonlySet<RunPhase> = new Set<RunPhase>([
  "completed",
  "cancelled",
  "failed",
  "killed",
]);

export type ChatEvent =
  | {
      kind: "run_status";
      run_id: string;
      phase: RunPhase;
      failure_summary: string | null;
    }
  | { kind: "gate"; run_id: string; gate_ref: string; headline: string; body: string }
  | { kind: "activity"; capability_id: string; status: string }
  | { kind: "stream_error"; reason: string };

export interface Message {
  sequence: number;
  /** "user" | "assistant" | "system" | "summary" | "other" */
  kind: string;
  content: string | null;
}

export interface SendResult {
  run_id: string;
}

export interface CancelResult {
  already_terminal: boolean;
}

/** A row in the sessions panel. Threads survive gateway restarts. */
export interface Thread {
  thread_id: string;
  /** `null` until the agent titles the thread. */
  title: string | null;
}

/**
 * A row in the automations panel. These are schedule entries, not run history.
 * `state` is a snake_case badge value; `last_status` is `null` before the first
 * run.
 */
export interface Automation {
  automation_id: string;
  name: string;
  state: string;
  next_run_at: string | null;
  last_run_at: string | null;
  last_status: string | null;
  is_active: boolean;
}

/** The sidecar's live state. `reason` is set only when `state` is "suspect". */
export interface SidecarState {
  state: "starting" | "loading" | "ready" | "restarting" | "suspect" | "stopped";
  reason?: string;
  attempt?: number;
  backoff_ms?: number;
}

/**
 * The local model panel. `local_model_status` returns `null` when the app runs
 * without local inference. `verdict` and `sidecar.state` are snake_case badge
 * values; sizes are MiB.
 */
export interface LocalModel {
  model_id: string;
  backend: string;
  sidecar: SidecarState;
  n_gpu_layers: number;
  block_count: number;
  verdict: "full_offload" | "partial_offload" | "cpu_only" | "refused";
  estimated_vram_mb: number;
  estimated_host_mb: number;
  warnings: string[];
}

/**
 * Which LLM the gateway runs on. Exactly one is active. `model` is an optional
 * override of the provider's default. Matches `ic_widget::settings`.
 */
export type ProviderSelection =
  | { kind: "local" }
  | { kind: "cloud"; id: string; model?: string | null };

/** A configurable cloud provider. `has_key` never carries the key itself. */
export interface Provider {
  id: string;
  description: string;
  default_model: string;
  has_key: boolean;
}

/** The provider panel's data: the active selection and the cloud catalog. */
export interface ProviderSettings {
  active: ProviderSelection;
  providers: Provider[];
}

/** A downloadable model the panel suggests. */
export interface RecommendedModel {
  id: string;
  name: string;
  repo: string;
  file: string;
  params: string;
  quant: string;
  approx_gib: number;
  note: string;
}

/** A model already on disk. `suspect` is the reason it is not auto-loaded. */
export interface InstalledModel {
  id: string;
  size_mb: number;
  suspect: string | null;
}

/** Emitted on `model://event` while a download runs. */
export type ModelEvent =
  | {
      kind: "progress";
      id: string;
      downloaded: number;
      total: number | null;
      fraction: number | null;
    }
  | { kind: "finished"; id: string; ok: boolean; cancelled: boolean; error: string | null };

/**
 * The character's animation state. Mirrors `ic_widget::character::CharacterState`
 * and is derived on the backend from the gateway's health and the active run.
 */
export type CharacterState =
  | "idle"
  | "listening"
  | "thinking"
  | "speaking"
  | "suggesting"
  | "concerned"
  | "error";

/** The active character, for the dashboard picker and the widget's renderer. */
export interface CharacterSettings {
  active: string;
  config_url: string;
}

/**
 * The click-through mask: a row-major LSB-first bitset of `cell`-sized squares
 * over the widget window, in logical pixels. Mirrors `ic_widget::hit_test`.
 */
export interface HitMask {
  cell: number;
  cols: number;
  rows: number;
  bits: number[];
}

/** Window-local cursor position (logical px), emitted while over the window. */
export interface CursorPos {
  x: number;
  y: number;
}

/** The local profile: who the user is, what the assistant is called, how it answers. */
export interface Profile {
  user_name: string;
  assistant_name: string;
  reply_mode: ReplyMode;
}

/** Whether a reply is shown, spoken, or both. */
export type ReplyMode = "read" | "hear" | "both";

/** Which microphone the app records from. */
export interface VoiceSettings {
  /** `null` follows the OS default — which is often a deaf Bluetooth headset. */
  input_device: string | null;
}

/**
 * Ambient mode: whether the character may speak first, and the guardrails on it.
 *
 * `enabled` is also what runs the gateway's trigger poller, so with it off a
 * scheduled automation is listed but never fires. Toggling it restarts the gateway.
 */
export interface AmbientStatus {
  enabled: boolean;
  /** Whether a completed task may earn a skill draft (Phase 7b). */
  reflection_enabled: boolean;
  /** Whether the watcher is live against a running gateway. */
  running: boolean;
  max_per_hour: number;
  /** Local hours; both `null` means no quiet window. */
  quiet_start: number | null;
  quiet_end: number | null;
}

/**
 * Something the character volunteered. Answer it with Accept or Not now — both are
 * recorded, and a "Not now" quiets that `source` for an hour.
 *
 * The `kind` decides what Accept does: an `automation` card opens the run's
 * thread; a `skill_draft` card **installs the draft in `body`** — it renders as
 * a red consent prompt, not a blue notification, and defaults to No.
 */
export interface Suggestion {
  id: string;
  kind: "automation" | "skill_draft";
  key: string;
  source: string;
  headline: string;
  body: string;
  /** The thread the detail lives in, when one could be identified. */
  thread_id: string | null;
}

/** What the app reports after an approved skill draft's install attempt. */
export interface SkillInstallResult {
  ok: boolean;
  name: string | null;
  error: string | null;
}

/** One recorded take of the wake phrase. */
export interface WakeSample {
  /** Loudness, so a muted microphone is caught on the first take, not the third. */
  peak: number;
  recorded: number;
  needed: number;
}

export const api = {
  gatewayState: () => invoke<GatewayState>("gateway_state"),
  /** The conversation both windows share, created on first ask. */
  currentThread: () => invoke<string>("current_thread"),
  /** Start a fresh conversation, in both windows. */
  newThread: () => invoke<string>("new_thread"),
  profile: () => invoke<Profile>("profile"),
  setProfile: (userName: string, assistantName: string, replyMode: ReplyMode) =>
    invoke<void>("set_profile", {
      userName,
      assistantName,
      replyMode,
    }),
  summonHotkey: () => invoke<string | null>("summon_hotkey"),
  setSummonHotkey: (binding: string) => invoke<void>("set_summon_hotkey", { binding }),
  /** Every input device, OS default first. */
  inputDevices: () => invoke<string[]>("input_devices"),
  setInputDevice: (device: string | null) => invoke<void>("set_input_device", { device }),
  /** Which microphone is chosen. */
  voiceSettings: () => invoke<VoiceSettings>("voice_settings"),
  /** Listen briefly and report the peak level — the only way to *see* a deaf mic. */
  testMicrophone: () => invoke<number>("test_microphone"),
  /** Record one utterance of the wake phrase (the assistant's name). */
  recordWakeSample: () => invoke<WakeSample>("record_wake_sample"),
  resetWakeSamples: () => invoke<void>("reset_wake_samples"),
  /** Start listening now — the button form of the summon hotkey. */
  startListening: () => invoke<void>("start_listening"),
  /** Speak a reply aloud. Rust ignores it unless the reply mode says to speak. */
  speakReply: (text: string) => invoke<void>("speak_reply", { text }),
  /** Whether a wake word has actually been trained (recording takes is not training). */
  hasWakeWord: () => invoke<boolean>("has_wake_word"),
  /** Turn the recordings into a wake-word model, on this machine. */
  trainWakeWord: (name: string) => invoke<string>("train_wake_word", { name }),
  sendMessage: (threadId: string, content: string) =>
    invoke<SendResult>("send_message", { threadId, content }),
  cancelRun: (threadId: string, runId: string) =>
    invoke<CancelResult>("cancel_run", { threadId, runId }),
  fetchTimeline: (threadId: string) => invoke<Message[]>("fetch_timeline", { threadId }),
  resolveGate: (threadId: string, runId: string, gateRef: string, approved: boolean) =>
    invoke<void>("resolve_gate", { threadId, runId, gateRef, approved }),
  openDashboard: () => invoke<void>("open_dashboard"),
  gatewayLog: () => invoke<string>("gateway_log"),
  listThreads: () => invoke<Thread[]>("list_threads"),
  listAutomations: () => invoke<Automation[]>("list_automations"),
  localModelStatus: () => invoke<LocalModel | null>("local_model_status"),
  providerSettings: () => invoke<ProviderSettings>("provider_settings"),
  setProviderKey: (providerId: string, key: string) =>
    invoke<void>("set_provider_key", { providerId, key }),
  clearProviderKey: (providerId: string) =>
    invoke<void>("clear_provider_key", { providerId }),
  applyProvider: (selection: ProviderSelection) =>
    invoke<void>("apply_provider", { selection }),
  recommendedModels: () => invoke<RecommendedModel[]>("recommended_models"),
  installedModels: () => invoke<InstalledModel[]>("installed_models"),
  downloadModel: (repo: string, file: string) =>
    invoke<void>("download_model", { repo, file }),
  cancelDownload: () => invoke<void>("cancel_download"),
  removeModel: (id: string) => invoke<void>("remove_model", { id }),
  characterState: () => invoke<CharacterState>("character_state"),
  characterSettings: () => invoke<CharacterSettings>("character_settings"),
  setCharacter: (id: string) => invoke<void>("set_character", { id }),
  setCharacterSignals: (signals: { listening?: boolean; speaking?: boolean }) =>
    invoke<void>("set_character_signals", signals),
  setHitMask: (mask: HitMask) => invoke<void>("set_hit_mask", { mask }),
  /** Answer a browser sensitive-fill approval request. */
  answerBrowserFill: (id: number, approved: boolean) =>
    invoke<void>("answer_browser_fill", { id, approved }),
  /** Begin an OS window drag — the character's body is the drag handle. */
  startDragging: () => getCurrentWindow().startDragging(),
  logUiError: (message: string) => invoke<void>("log_ui_error", { message }),
  /** Whether voice is enabled, actually running, and muted. */
  voiceStatus: () => invoke<VoiceStatus>("voice_status"),
  /** Mute or unmute the microphone. */
  setVoiceMuted: (muted: boolean) => invoke<void>("set_voice_muted", { muted }),
  /** Turn voice on or off (enabling downloads the speech models on first run). */
  setVoiceEnabled: (enabled: boolean) => invoke<void>("set_voice_enabled", { enabled }),
  /** Whether the first-run setup wizard should be shown. */
  needsSetup: () => invoke<boolean>("needs_setup"),
  /** Mark first-run setup complete. */
  completeSetup: () => invoke<void>("complete_setup"),
  /** Whether the character may speak first, and under what limits. */
  ambientStatus: () => invoke<AmbientStatus>("ambient_status"),
  /** Turn ambient mode on or off. Restarts the gateway (the trigger poller is a
   *  boot-time switch), so both windows reload. */
  setAmbientEnabled: (enabled: boolean) => invoke<void>("set_ambient_enabled", { enabled }),
  /** Change the interruption limits. Takes effect on the next tick, no restart. */
  setAmbientGuardrails: (
    maxPerHour: number,
    quietStart: number | null,
    quietEnd: number | null,
  ) => invoke<void>("set_ambient_guardrails", { maxPerHour, quietStart, quietEnd }),
  /** Turn the reflection pass on or off. No restart. */
  setReflectionEnabled: (enabled: boolean) =>
    invoke<void>("set_reflection_enabled", { enabled }),
  /** Answer a suggestion. Accept opens the run's thread — or, for a skill
   *  draft, installs it. Both answers are recorded. */
  respondSuggestion: (id: string, accepted: boolean) =>
    invoke<void>("respond_suggestion", { id, accepted }),
};

/** Subscribe to suggestions the character volunteers. */
export function onAmbientSuggestion(
  handler: (suggestion: Suggestion) => void,
): Promise<UnlistenFn> {
  return listen<Suggestion>("ambient://suggestion", (event) => handler(event.payload));
}

/** Subscribe to the result of installing an approved skill draft. */
export function onSkillInstallResult(
  handler: (result: SkillInstallResult) => void,
): Promise<UnlistenFn> {
  return listen<SkillInstallResult>("ambient://install-result", (event) =>
    handler(event.payload),
  );
}

/** The voice UI status (mirrors the Rust `VoiceStatus`). */
export interface VoiceStatus {
  enabled: boolean;
  running: boolean;
  muted: boolean;
}

/**
 * A request to type into a sensitive field, needing the user's OK first.
 *
 * Enforced in the browser sidecar — the runtime's own approval flow does not run
 * (see `docs/desktop/core-patches.md`), so this is the real gate on the agent
 * typing a password or card number. Shows what will be typed and where, because a
 * consent prompt the user can't evaluate isn't consent.
 */
export interface BrowserApproval {
  id: number;
  url: string;
  secure: boolean;
  field: string;
  selector: string;
  value: string;
  reason: string;
}

/** Subscribe to browser sensitive-fill approval requests. */
export function onBrowserApproval(
  handler: (request: BrowserApproval) => void,
): Promise<UnlistenFn> {
  return listen<BrowserApproval>("browser://approval", (event) => handler(event.payload));
}

/** Agent-authored markup for the canvas window. Untrusted — render only inside a
 *  locked-down sandbox iframe. */
export interface CanvasRender {
  html: string;
  title: string | null;
}

/** The latest canvas render, read on mount to cover the event-before-listener
 *  race when the window first opens. */
export function canvasContent(): Promise<CanvasRender | null> {
  return invoke<CanvasRender | null>("canvas_content");
}

/** Subscribe to live canvas renders. */
export function onCanvasRender(handler: (render: CanvasRender) => void): Promise<UnlistenFn> {
  return listen<CanvasRender>("canvas://render", (event) => handler(event.payload));
}

/** Subscribe to model-download progress and completion. */
export function onModelEvent(handler: (event: ModelEvent) => void): Promise<UnlistenFn> {
  return listen<ModelEvent>("model://event", (event) => handler(event.payload));
}

/** Subscribe to character animation-state changes. */
export function onCharacterState(handler: (state: CharacterState) => void): Promise<UnlistenFn> {
  return listen<CharacterState>("character://state", (event) => handler(event.payload));
}

/** Subscribe to gateway health transitions. */
export function onGatewayState(handler: (state: GatewayState) => void): Promise<UnlistenFn> {
  return listen<GatewayState>("gateway://state", (event) => handler(event.payload));
}

/** Subscribe to the chat event pump for the active thread. */
export function onChatEvent(handler: (event: ChatEvent) => void): Promise<UnlistenFn> {
  return listen<ChatEvent>("chat://event", (event) => handler(event.payload));
}

/**
 * The shared conversation was replaced (someone started a new session). Both
 * windows follow it — otherwise the widget keeps bubbling replies from a thread
 * the user has left.
 */
export function onThreadChanged(handler: (threadId: string) => void): Promise<UnlistenFn> {
  return listen<string>("thread://changed", (event) => handler(event.payload));
}

/**
 * What the microphone heard, after every transcription — **including an empty
 * string**, which means "I listened and heard nothing". That is a different and far
 * more useful answer than silence, which is what the user got before.
 */
export function onVoiceTranscript(handler: (text: string) => void): Promise<UnlistenFn> {
  return listen<string>("voice://transcript", (event) => handler(event.payload));
}

/** The profile changed in the other window (name, or how replies are delivered). */
export function onProfileChanged(handler: (profile: Profile) => void): Promise<UnlistenFn> {
  return listen<Profile>("profile://changed", (event) => handler(event.payload));
}

/**
 * Subscribe to the global cursor while it is over the widget window. This is
 * how eye tracking works even when the window is click-through: the webview
 * receives no mouse events then, so Rust polls the cursor and reports it here.
 */
export function onCursorPos(handler: (pos: CursorPos) => void): Promise<UnlistenFn> {
  return listen<CursorPos>("cursor://pos", (event) => handler(event.payload));
}

/**
 * Subscribe to the animation gate: `false` while a fullscreen app is
 * foreground, so the idle character never competes with it for the GPU.
 */
export function onCharacterActive(handler: (active: boolean) => void): Promise<UnlistenFn> {
  return listen<boolean>("character://active", (event) => handler(event.payload));
}

/** Where the voice loop is (mirrors `ic_voice::VoiceState`). */
export type VoiceState =
  | "muted"
  | "idle"
  | "listening"
  | "transcribing"
  | "sending"
  | "speaking";

/**
 * Subscribe to the TTS playback amplitude (0..1) while the character speaks. The
 * widget computes it from Piper's output RMS; the renderer drives `ParamMouthOpenY`
 * from it for real lip sync.
 */
export function onVoiceAmplitude(handler: (level: number) => void): Promise<UnlistenFn> {
  return listen<number>("voice://amplitude", (event) => handler(event.payload));
}

/** Subscribe to voice-loop state changes, for the mic-live indicator. */
export function onVoiceState(handler: (state: VoiceState) => void): Promise<UnlistenFn> {
  return listen<VoiceState>("voice://state", (event) => handler(event.payload));
}
