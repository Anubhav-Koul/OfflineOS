/* @refresh reload */
import { render } from "solid-js/web";
import { createSignal, For, onCleanup, onMount, Show } from "solid-js";

import {
  api,
  onChatEvent,
  onCharacterState,
  onGatewayState,
  TERMINAL_PHASES,
  type ChatEvent,
  type GatewayState,
  type Message,
  type RunPhase,
} from "./api";
import {
  createRenderer,
  loadCharacterConfig,
  PLACEHOLDER_CONFIG,
  type CharacterRenderer,
} from "./character";
import "./styles.css";

/**
 * The always-on-top chat widget.
 *
 * The interesting part is how a reply arrives. The gateway's event stream never
 * carries assistant text — see `docs/desktop/chat-rendering.md`. So:
 *
 *   1. send the message, remember the `run_id`
 *   2. watch `run_status` events until the run is terminal
 *   3. *then* fetch the timeline and render the assistant's message
 *
 * Everything before step 3 is status, not content.
 */

interface Bubble {
  role: "user" | "assistant" | "error";
  text: string;
}

interface Gate {
  runId: string;
  gateRef: string;
  headline: string;
  body: string;
}

function App() {
  const [threadId, setThreadId] = createSignal<string | null>(null);
  const [bubbles, setBubbles] = createSignal<Bubble[]>([]);
  const [draft, setDraft] = createSignal("");
  const [activeRun, setActiveRun] = createSignal<string | null>(null);
  const [phase, setPhase] = createSignal<RunPhase | null>(null);
  const [activity, setActivity] = createSignal<string | null>(null);
  const [gate, setGate] = createSignal<Gate | null>(null);
  const [gateway, setGateway] = createSignal<GatewayState>({ state: "starting" });

  let transcript: HTMLDivElement | undefined;
  const scrollToEnd = () =>
    queueMicrotask(() => transcript?.scrollTo({ top: transcript.scrollHeight }));

  const push = (bubble: Bubble) => {
    setBubbles((current) => [...current, bubble]);
    scrollToEnd();
  };

  /** The run finished. Its text is in the timeline, nowhere else. */
  async function collectReply(failureSummary: string | null) {
    const id = threadId();
    setActiveRun(null);
    setActivity(null);
    setGate(null);

    if (failureSummary) {
      push({ role: "error", text: failureSummary });
      return;
    }
    if (!id) return;

    try {
      const messages: Message[] = await api.fetchTimeline(id);
      const reply = [...messages]
        .reverse()
        .find((message) => message.kind === "assistant" && message.content);
      if (reply?.content) push({ role: "assistant", text: reply.content });
    } catch (error) {
      push({ role: "error", text: `Could not load the reply: ${error}` });
    }
  }

  function handleChatEvent(event: ChatEvent) {
    switch (event.kind) {
      case "run_status": {
        if (event.run_id !== activeRun()) return;
        setPhase(event.phase);
        if (TERMINAL_PHASES.has(event.phase)) {
          const failed = event.phase === "failed" || event.phase === "killed";
          void collectReply(
            failed ? (event.failure_summary ?? "The turn failed.") : null,
          );
        }
        break;
      }
      case "gate":
        // The agent wants to run a tool. It stays parked until answered.
        setGate({
          runId: event.run_id,
          gateRef: event.gate_ref,
          headline: event.headline,
          body: event.body,
        });
        break;
      case "activity":
        setActivity(event.status === "completed" ? null : event.capability_id);
        break;
      case "stream_error":
        push({ role: "error", text: event.reason });
        setActiveRun(null);
        break;
    }
  }

  /**
   * Create the thread once, and only once the gateway is ready.
   *
   * The widget paints before the gateway finishes booting — a first run migrates
   * the database and installs bundled skills. Creating the thread on mount would
   * fail with "still starting" and never retry, so the thread is created on the
   * first `ready` we observe, whether that arrives as an event or as the initial
   * state we read below.
   */
  let creating = false;
  async function ensureThread(state: GatewayState) {
    if (state.state !== "ready" || threadId() || creating) return;
    creating = true;
    try {
      setThreadId(await api.createThread());
    } catch (error) {
      push({ role: "error", text: `Could not start a conversation: ${error}` });
    } finally {
      creating = false;
    }
  }

  onMount(async () => {
    const unlistenState = await onGatewayState((state) => {
      setGateway(state);
      void ensureThread(state);
    });
    const unlistenChat = await onChatEvent(handleChatEvent);
    onCleanup(() => {
      unlistenState();
      unlistenChat();
    });

    try {
      // Covers the race where the gateway became ready before we subscribed.
      const initial = await api.gatewayState();
      setGateway(initial);
      await ensureThread(initial);
    } catch (error) {
      push({ role: "error", text: `The agent is not reachable: ${error}` });
    }
  });

  async function send(event: SubmitEvent) {
    event.preventDefault();
    const text = draft().trim();
    const id = threadId();
    if (!text || !id || activeRun()) return;

    setDraft("");
    push({ role: "user", text });
    setPhase("queued");
    try {
      const { run_id } = await api.sendMessage(id, text);
      setActiveRun(run_id);
    } catch (error) {
      setPhase(null);
      push({ role: "error", text: `Could not send: ${error}` });
    }
  }

  async function stop() {
    const id = threadId();
    const run = activeRun();
    if (!id || !run) return;
    try {
      // The Stop button races the answer; `already_terminal` means the reply
      // landed first, which is not a failure.
      const { already_terminal } = await api.cancelRun(id, run);
      if (already_terminal) await collectReply(null);
    } catch (error) {
      push({ role: "error", text: `Could not stop: ${error}` });
    }
  }

  async function answerGate(approved: boolean) {
    const pending = gate();
    const id = threadId();
    if (!pending || !id) return;
    setGate(null);
    try {
      await api.resolveGate(id, pending.runId, pending.gateRef, approved);
    } catch (error) {
      push({ role: "error", text: `Could not answer: ${error}` });
    }
  }

  const busy = () => activeRun() !== null;
  const statusLine = () => {
    const current = phase();
    if (gate()) return "waiting for you";
    if (activity()) return `running ${activity()}`;
    if (!current || TERMINAL_PHASES.has(current)) return null;
    if (current === "queued") return "queued";
    if (current === "cancel_requested") return "stopping";
    if (current.startsWith("blocked")) return "waiting for you";
    return "thinking";
  };

  return (
    <div class="widget">
      {/* Undecorated window: this strip is what the user drags. */}
      <header class="widget-header" data-tauri-drag-region>
        <span class="title" data-tauri-drag-region>
          IronClaw
        </span>
        <HealthBadge state={gateway()} />
        <button class="ghost" title="Dashboard" onClick={() => void api.openDashboard()}>
          ⋯
        </button>
      </header>

      <CharacterView />

      <div class="transcript" ref={transcript}>
        <For each={bubbles()}>
          {(bubble) => <div class={`bubble ${bubble.role}`}>{bubble.text}</div>}
        </For>

        <Show when={statusLine()}>
          {(line) => <div class="status">{line()}</div>}
        </Show>

        <Show when={gate()}>
          {(pending) => (
            <div class="gate">
              <div class="gate-headline">{pending().headline}</div>
              <pre class="gate-body">{pending().body}</pre>
              <div class="gate-actions">
                <button class="approve" onClick={() => void answerGate(true)}>
                  Allow once
                </button>
                <button class="deny" onClick={() => void answerGate(false)}>
                  Deny
                </button>
              </div>
            </div>
          )}
        </Show>
      </div>

      <form class="composer" onSubmit={send}>
        <input
          type="text"
          placeholder={busy() ? "Working…" : "Ask something"}
          value={draft()}
          disabled={busy() || gateway().state !== "ready"}
          onInput={(event) => setDraft(event.currentTarget.value)}
        />
        <Show
          when={busy()}
          fallback={
            <button type="submit" disabled={!draft().trim() || gateway().state !== "ready"}>
              Send
            </button>
          }
        >
          {/* Always visible while a run is in flight, per the project's
              runaway-loop guardrails. */}
          <button type="button" class="stop" onClick={() => void stop()}>
            Stop
          </button>
        </Show>
      </form>
    </div>
  );
}

function HealthBadge(props: { state: GatewayState }) {
  const label = () => {
    switch (props.state.state) {
      case "ready":
        return "ready";
      case "starting":
        return "starting";
      case "restarting":
        return `restarting (${props.state.attempt})`;
      case "unhealthy":
        return "unhealthy";
      case "stopped":
        return "stopped";
    }
  };
  const title = () =>
    props.state.state === "unhealthy" ? props.state.reason : `Gateway is ${label()}`;

  return (
    <span class={`badge ${props.state.state}`} title={title()}>
      {label()}
    </span>
  );
}

/**
 * The character. Owns a {@link CharacterRenderer} and drives it from
 * `character://state`, reading the current state on mount to cover the race
 * where it changed before we subscribed.
 */
const REN_CONFIG_URL = "/characters/ren/character.json";

function CharacterView() {
  let container: HTMLDivElement | undefined;

  onMount(async () => {
    if (!container) return;

    // Try the Live2D character; on any failure — missing Core, a Cubism 5.3
    // feature WebView2 will not render, a bad asset path — fall back to the
    // placeholder so the character still reacts and the console carries the
    // reason. This is the Phase 3 compatibility check in practice.
    let renderer: CharacterRenderer;
    try {
      const config = await loadCharacterConfig(REN_CONFIG_URL);
      renderer = createRenderer(config);
      await renderer.mount(container);
    } catch (error) {
      console.error("[character] Live2D failed to load; using placeholder", error);
      renderer = createRenderer(PLACEHOLDER_CONFIG);
      await renderer.mount(container);
    }

    const unlisten = await onCharacterState((state) => renderer.setState(state));
    renderer.setState(await api.characterState());

    onCleanup(() => {
      unlisten();
      renderer.destroy();
    });
  });

  return <div class="character" ref={container} />;
}

const root = document.getElementById("root");
if (root) render(() => <App />, root);
