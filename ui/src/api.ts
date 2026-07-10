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
};

/** Subscribe to gateway health transitions. */
export function onGatewayState(handler: (state: GatewayState) => void): Promise<UnlistenFn> {
  return listen<GatewayState>("gateway://state", (event) => handler(event.payload));
}

/** Subscribe to the chat event pump for the active thread. */
export function onChatEvent(handler: (event: ChatEvent) => void): Promise<UnlistenFn> {
  return listen<ChatEvent>("chat://event", (event) => handler(event.payload));
}
