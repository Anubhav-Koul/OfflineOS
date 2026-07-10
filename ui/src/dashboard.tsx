/* @refresh reload */
import { render } from "solid-js/web";
import { createSignal, For, onCleanup, onMount, Show } from "solid-js";

import {
  api,
  onGatewayState,
  type Automation,
  type GatewayState,
  type LocalModel,
  type Thread,
} from "./api";
import "./styles.css";

/**
 * The dashboard.
 *
 * Panels are split by what the `serve` API can actually back. Sessions
 * (`GET /threads`) and automations (`GET /automations`) have live routes.
 * The memory browser, skills list, audit log, and run history have **no HTTP
 * route** in `ironclaw-reborn serve` — see `docs/desktop/dashboard-gaps.md`.
 * They are listed as explicitly unavailable rather than faked.
 */

const UNAVAILABLE_PANELS = [
  {
    name: "Memory browser",
    reason: "no memory route exists in the serve API",
  },
  {
    name: "Skills list",
    reason: "skills are an in-agent tool, not an HTTP route",
  },
  {
    name: "Audit log",
    reason: "audit records go to internal sinks, not an HTTP route",
  },
  {
    name: "Run history",
    reason: "automations expose schedules only, not past runs",
  },
] as const;

/**
 * A panel whose data is loaded on demand from a command that needs the gateway.
 *
 * Tracks the three states the UI must tell apart: never-loaded, in-flight, and
 * failed. A command rejects with a friendly string while the gateway is still
 * starting, so `error` is shown as-is — it is already user-facing.
 */
function createPanelData<T>(load: () => Promise<T[]>) {
  const [rows, setRows] = createSignal<T[]>([]);
  const [error, setError] = createSignal<string | null>(null);
  const [loading, setLoading] = createSignal(false);
  const [loaded, setLoaded] = createSignal(false);

  const refresh = async () => {
    setLoading(true);
    setError(null);
    try {
      setRows(await load());
      setLoaded(true);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setLoading(false);
    }
  };

  return { rows, error, loading, loaded, refresh };
}

/** The single-value sibling of {@link createPanelData}, for a panel that shows
 * one record (or none) rather than a list. */
function createValueData<T>(load: () => Promise<T>) {
  const [value, setValue] = createSignal<T | undefined>(undefined);
  const [error, setError] = createSignal<string | null>(null);
  const [loading, setLoading] = createSignal(false);
  const [loaded, setLoaded] = createSignal(false);

  const refresh = async () => {
    setLoading(true);
    setError(null);
    try {
      const next = await load();
      setValue(() => next);
      setLoaded(true);
    } catch (reason) {
      setError(String(reason));
    } finally {
      setLoading(false);
    }
  };

  return { value, error, loading, loaded, refresh };
}

function Dashboard() {
  const [gateway, setGateway] = createSignal<GatewayState>({ state: "starting" });
  const [log, setLog] = createSignal("");

  const sessions = createPanelData<Thread>(api.listThreads);
  const automations = createPanelData<Automation>(api.listAutomations);
  const model = createValueData<LocalModel | null>(api.localModelStatus);

  const loadAll = () => {
    void sessions.refresh();
    void automations.refresh();
    void model.refresh();
  };

  onMount(async () => {
    const unlisten = await onGatewayState((state) => {
      const wasReady = gateway().state === "ready";
      setGateway(state);
      // Panels can only load once the gateway answers. Load on the first
      // ready we see, not on mount — the gateway takes ~500 ms to boot.
      if (state.state === "ready" && !wasReady) loadAll();
    });
    onCleanup(unlisten);

    const current = await api.gatewayState();
    setGateway(current);
    setLog(await api.gatewayLog());
    if (current.state === "ready") loadAll();
  });

  const refreshLog = async () => setLog(await api.gatewayLog());

  return (
    <div class="dashboard">
      <h1>IronClaw Desktop</h1>

      <section>
        <h2>Gateway</h2>
        <p>
          <span class={`badge ${gateway().state}`}>{gateway().state}</span>
        </p>
        <Show when={gateway().state === "unhealthy"}>
          <pre class="reason">
            {gateway().state === "unhealthy" ? (gateway() as { reason: string }).reason : ""}
          </pre>
        </Show>
        <button onClick={() => void refreshLog()}>Refresh log</button>
        <pre class="log">{log() || "(no output yet)"}</pre>
      </section>

      <section>
        <div class="panel-head">
          <h2>Sessions</h2>
          <button class="ghost" disabled={sessions.loading()} onClick={() => void sessions.refresh()}>
            Refresh
          </button>
        </div>
        <PanelBody
          error={sessions.error()}
          loading={sessions.loading()}
          loaded={sessions.loaded()}
          empty={sessions.rows().length === 0}
          emptyText="No conversations yet."
        >
          <ul class="rows">
            <For each={sessions.rows()}>
              {(thread) => (
                <li class="row">
                  <span class="row-title">{thread.title ?? "Untitled conversation"}</span>
                  <span class="row-meta">{thread.thread_id}</span>
                </li>
              )}
            </For>
          </ul>
        </PanelBody>
      </section>

      <section>
        <div class="panel-head">
          <h2>Automations</h2>
          <button
            class="ghost"
            disabled={automations.loading()}
            onClick={() => void automations.refresh()}
          >
            Refresh
          </button>
        </div>
        <p class="muted small">
          Scheduled entries only — <code>serve</code> exposes no run history.
        </p>
        <PanelBody
          error={automations.error()}
          loading={automations.loading()}
          loaded={automations.loaded()}
          empty={automations.rows().length === 0}
          emptyText="No automations scheduled."
        >
          <ul class="rows">
            <For each={automations.rows()}>
              {(automation) => (
                <li class="row">
                  <span class="row-title">{automation.name}</span>
                  <span class={`badge ${badgeClass(automation.state)}`}>{automation.state}</span>
                  <span class="row-meta">
                    {automation.next_run_at
                      ? `next ${formatWhen(automation.next_run_at)}`
                      : automation.last_run_at
                        ? `last ${formatWhen(automation.last_run_at)}`
                        : "never run"}
                    <Show when={automation.last_status}>
                      {(status) => <> · {status()}</>}
                    </Show>
                  </span>
                </li>
              )}
            </For>
          </ul>
        </PanelBody>
      </section>

      <section>
        <div class="panel-head">
          <h2>Local model</h2>
          <button class="ghost" disabled={model.loading()} onClick={() => void model.refresh()}>
            Refresh
          </button>
        </div>
        <Show
          when={!model.error()}
          fallback={<p class="reason-inline">{model.error()}</p>}
        >
          <Show
            when={model.loaded()}
            fallback={<p class="muted">{model.loading() ? "Loading…" : ""}</p>}
          >
            <Show
              when={model.value()}
              fallback={
                <p class="muted">
                  No local model running — the app is using a configured cloud provider, or no
                  model is installed yet.
                </p>
              }
            >
              {(m) => (
                <div class="model">
                  <div class="row">
                    <span class="row-title">{m().model_id}</span>
                    <span class={`badge ${sidecarBadgeClass(m().sidecar.state)}`}>
                      {m().sidecar.state}
                    </span>
                  </div>
                  <Show when={m().sidecar.reason}>
                    {(reason) => <p class="reason-inline">{reason()}</p>}
                  </Show>
                  <dl class="facts">
                    <div>
                      <dt>Backend</dt>
                      <dd>{m().backend}</dd>
                    </div>
                    <div>
                      <dt>Offload</dt>
                      <dd>{offloadLabel(m().verdict)}</dd>
                    </div>
                    <div>
                      <dt>GPU layers</dt>
                      <dd>
                        {m().n_gpu_layers} / {m().block_count}
                      </dd>
                    </div>
                    <div>
                      <dt>Est. VRAM</dt>
                      <dd>{m().estimated_vram_mb} MiB</dd>
                    </div>
                    <div>
                      <dt>Est. RAM</dt>
                      <dd>{m().estimated_host_mb} MiB</dd>
                    </div>
                  </dl>
                  <Show when={m().warnings.length > 0}>
                    <ul class="warnings">
                      <For each={m().warnings}>{(warning) => <li>{warning}</li>}</For>
                    </ul>
                  </Show>
                </div>
              )}
            </Show>
          </Show>
        </Show>
      </section>

      <section>
        <h2>Not available yet</h2>
        <p class="muted">
          These panels are in the plan but have no endpoint in{" "}
          <code>ironclaw-reborn serve</code>. They are listed rather than faked.
        </p>
        <ul>
          {UNAVAILABLE_PANELS.map((panel) => (
            <li>
              <strong>{panel.name}</strong> — <span class="muted">{panel.reason}</span>
            </li>
          ))}
        </ul>
      </section>
    </div>
  );
}

/** The shared shell around a panel's rows: error, loading, empty, or content. */
function PanelBody(props: {
  error: string | null;
  loading: boolean;
  loaded: boolean;
  empty: boolean;
  emptyText: string;
  children: unknown;
}) {
  return (
    <Show
      when={!props.error}
      fallback={<p class="reason-inline">{props.error}</p>}
    >
      <Show
        when={props.loaded && !props.empty}
        fallback={
          <p class="muted">{props.loading && !props.loaded ? "Loading…" : props.emptyText}</p>
        }
      >
        {props.children as never}
      </Show>
    </Show>
  );
}

/** Map a wire state onto one of the existing badge colour classes. */
function badgeClass(state: string): string {
  switch (state) {
    case "active":
    case "scheduled":
      return "ready";
    case "paused":
    case "inactive":
    case "completed":
      return "starting";
    case "disabled":
      return "stopped";
    default:
      return "";
  }
}

/** Map a sidecar state onto one of the existing badge colour classes. */
function sidecarBadgeClass(state: string): string {
  switch (state) {
    case "ready":
      return "ready";
    case "loading":
    case "starting":
    case "restarting":
      return "starting";
    case "suspect":
      return "unhealthy";
    case "stopped":
      return "stopped";
    default:
      return "";
  }
}

/** A plain-language label for a placement verdict. */
function offloadLabel(verdict: string): string {
  switch (verdict) {
    case "full_offload":
      return "Full (GPU)";
    case "partial_offload":
      return "Partial (GPU + CPU)";
    case "cpu_only":
      return "CPU only";
    case "refused":
      return "Refused";
    default:
      return verdict;
  }
}

/** Render an RFC 3339 timestamp in the user's locale, or pass it through. */
function formatWhen(iso: string): string {
  const date = new Date(iso);
  return Number.isNaN(date.getTime()) ? iso : date.toLocaleString();
}

const root = document.getElementById("root");
if (root) render(() => <Dashboard />, root);
