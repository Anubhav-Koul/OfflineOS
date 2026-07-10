/* @refresh reload */
import { render } from "solid-js/web";
import { createSignal, onCleanup, onMount, Show } from "solid-js";

import { api, onGatewayState, type GatewayState } from "./api";
import "./styles.css";

/**
 * The dashboard.
 *
 * Phase 2a ships the panels the `serve` API can actually back: gateway health
 * and its log. The memory browser, skills list, and audit-log viewer named in
 * the project plan have **no HTTP route** in `ironclaw-reborn serve` — see
 * `docs/desktop/dashboard-gaps.md`. They are listed here as explicitly
 * unavailable rather than faked.
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
] as const;

function Dashboard() {
  const [gateway, setGateway] = createSignal<GatewayState>({ state: "starting" });
  const [log, setLog] = createSignal("");

  onMount(async () => {
    const unlisten = await onGatewayState(setGateway);
    onCleanup(unlisten);
    setGateway(await api.gatewayState());
    setLog(await api.gatewayLog());
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

const root = document.getElementById("root");
if (root) render(() => <Dashboard />, root);
