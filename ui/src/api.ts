import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

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
  | "concerned"
  | "error";

export const api = {
  gatewayState: () => invoke<GatewayState>("gateway_state"),
  createThread: () => invoke<string>("create_thread"),
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
};

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
