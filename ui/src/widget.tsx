/* @refresh reload */
import { render } from "solid-js/web";
import {
  createEffect,
  createSignal,
  For,
  on,
  onCleanup,
  onMount,
  Show,
} from "solid-js";

import {
  api,
  onBrowserApproval,
  onCharacterActive,
  onCharacterState,
  onChatEvent,
  onCursorPos,
  onGatewayState,
  onVoiceAmplitude,
  onVoiceState,
  TERMINAL_PHASES,
  type BrowserApproval,
  type ChatEvent,
  type GatewayState,
  type HitMask,
  type Message,
  type RunPhase,
  type VoiceState,
} from "./api";
import {
  createRenderer,
  loadCharacterConfig,
  PLACEHOLDER_CONFIG,
  type CharacterRenderer,
  type HitProfile,
} from "./character";
import "./styles.css";

/**
 * Mirror console errors/warnings and unhandled failures to the backend log.
 *
 * Everything below the app — Pixi, pixi-live2d-display, the Cubism Framework —
 * reports failures via `console.error`, which lands in the webview's console: an
 * awkward place to reach in a transparent borderless widget. The stderr log is
 * where the gateway and sidecar already report, so failures belong there too.
 */
function bridgeConsoleToLog() {
  const render = (value: unknown): string => {
    if (value instanceof Error) return value.stack ?? value.message;
    if (typeof value === "string") return value;
    try {
      return JSON.stringify(value);
    } catch {
      return String(value);
    }
  };
  for (const level of ["error", "warn"] as const) {
    const original = console[level].bind(console);
    console[level] = (...args: unknown[]) => {
      original(...args);
      const text = args.map(render).join(" ").slice(0, 1000);
      void api.logUiError(`console.${level}: ${text}`).catch(() => undefined);
    };
  }
  window.addEventListener("error", (event) =>
    void api.logUiError(
      `window.onerror: ${event.message} @${event.filename}:${event.lineno}:${event.colno}\n${
        event.error instanceof Error ? (event.error.stack ?? "") : ""
      }`,
    ),
  );
  window.addEventListener("unhandledrejection", (event) =>
    void api.logUiError(`unhandledrejection: ${render(event.reason)}`),
  );
}
bridgeConsoleToLog();

/**
 * The desktop companion window: the character standing at the bottom, the chat
 * panel — the Phase 2 speech bubble — anchored above it. Clicking the
 * character's head toggles the panel; dragging its body moves the window;
 * everything outside both passes through to the desktop (the hit mask below).
 *
 * How a reply arrives is unchanged from Phase 2a. The gateway's event stream
 * never carries assistant text — see `docs/desktop/chat-rendering.md`. So:
 *
 *   1. send the message, remember the `run_id`
 *   2. watch `run_status` events until the run is terminal
 *   3. *then* fetch the timeline and render the assistant's message
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

/** Cell size of the click-through mask, logical px. */
const MASK_CELL = 8;

/**
 * Rasterize the solid parts of the window — the chat panel's DOM rect plus the
 * character's silhouette — into the packed grid `ic_widget::hit_test` expects.
 * Dilated by one cell: the silhouette moves between refreshes, and a click that
 * grazes an edge should land on the character, not the desktop.
 */
function buildHitMask(profile: HitProfile | null, elements: HTMLElement[]): HitMask {
  const cols = Math.max(1, Math.ceil(window.innerWidth / MASK_CELL));
  const rows = Math.max(1, Math.ceil(window.innerHeight / MASK_CELL));
  const grid = new Uint8Array(cols * rows);

  const markRect = (left: number, top: number, width: number, height: number) => {
    if (width <= 0 || height <= 0) return;
    const c0 = Math.max(0, Math.floor(left / MASK_CELL));
    const r0 = Math.max(0, Math.floor(top / MASK_CELL));
    const c1 = Math.min(cols - 1, Math.floor((left + width - 1) / MASK_CELL));
    const r1 = Math.min(rows - 1, Math.floor((top + height - 1) / MASK_CELL));
    for (let r = r0; r <= r1; r++) {
      for (let c = c0; c <= c1; c++) grid[r * cols + c] = 1;
    }
  };

  for (const element of elements) {
    const rect = element.getBoundingClientRect();
    markRect(rect.left, rect.top, rect.width, rect.height);
  }
  if (profile?.kind === "rect") {
    markRect(profile.left, profile.top, profile.width, profile.height);
  } else if (profile) {
    for (let r = 0; r < profile.rows; r++) {
      for (let c = 0; c < profile.cols; c++) {
        if (!profile.solid[r * profile.cols + c]) continue;
        markRect(
          profile.originX + c * profile.cell,
          profile.originY + r * profile.cell,
          profile.cell,
          profile.cell,
        );
      }
    }
  }

  const dilated = new Uint8Array(grid);
  for (let r = 0; r < rows; r++) {
    for (let c = 0; c < cols; c++) {
      if (!grid[r * cols + c]) continue;
      for (let dr = -1; dr <= 1; dr++) {
        for (let dc = -1; dc <= 1; dc++) {
          const rr = r + dr;
          const cc = c + dc;
          if (rr >= 0 && rr < rows && cc >= 0 && cc < cols) dilated[rr * cols + cc] = 1;
        }
      }
    }
  }

  const bits = new Uint8Array(Math.ceil((cols * rows) / 8));
  for (let i = 0; i < dilated.length; i++) {
    if (dilated[i]) bits[i >> 3]! |= 1 << (i & 7);
  }
  return { cell: MASK_CELL, cols, rows, bits: Array.from(bits) };
}

function App() {
  const [threadId, setThreadId] = createSignal<string | null>(null);
  const [bubbles, setBubbles] = createSignal<Bubble[]>([]);
  const [draft, setDraft] = createSignal("");
  const [activeRun, setActiveRun] = createSignal<string | null>(null);
  const [phase, setPhase] = createSignal<RunPhase | null>(null);
  const [activity, setActivity] = createSignal<string | null>(null);
  const [gate, setGate] = createSignal<Gate | null>(null);
  // Sensitive-fill approvals queue: a page may ask for several in a row, and each
  // must be answered explicitly, so they are not collapsed into one.
  const [fills, setFills] = createSignal<BrowserApproval[]>([]);
  const [gateway, setGateway] = createSignal<GatewayState>({ state: "starting" });
  const [panelOpen, setPanelOpen] = createSignal(true);

  let transcript: HTMLDivElement | undefined;
  const scrollToEnd = () =>
    queueMicrotask(() => transcript?.scrollTo({ top: transcript.scrollHeight }));

  const push = (bubble: Bubble) => {
    setBubbles((current) => [...current, bubble]);
    scrollToEnd();
  };

  /**
   * The `speaking` signal's off-switch. The character speaks while a reply is
   * rendered; with no TTS yet the window is a reading-time estimate, replaced
   * by real playback in Phase 5.
   */
  let speakingTimer: ReturnType<typeof setTimeout> | undefined;
  const setSpeaking = (text: string | null) => {
    clearTimeout(speakingTimer);
    if (text === null) {
      void api.setCharacterSignals({ speaking: false }).catch(() => undefined);
      return;
    }
    void api.setCharacterSignals({ speaking: true }).catch(() => undefined);
    const readingMs = Math.min(2000 + text.length * 40, 10_000);
    speakingTimer = setTimeout(() => {
      void api.setCharacterSignals({ speaking: false }).catch(() => undefined);
    }, readingMs);
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
      if (reply?.content) {
        push({ role: "assistant", text: reply.content });
        setSpeaking(reply.content);
      }
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
        // The agent wants to run a tool. It stays parked until answered — and
        // the panel opens so the prompt is actually visible, mirroring the
        // character's `concerned` face.
        setGate({
          runId: event.run_id,
          gateRef: event.gate_ref,
          headline: event.headline,
          body: event.body,
        });
        setPanelOpen(true);
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
    const unlistenFill = await onBrowserApproval((request) => {
      // A sensitive fill needs a decision now; force the panel open so the prompt
      // is visible even if the user had collapsed it, the same as a tool gate.
      setFills((current) => [...current, request]);
      setPanelOpen(true);
    });
    onCleanup(() => {
      unlistenState();
      unlistenChat();
      unlistenFill();
      clearTimeout(speakingTimer);
    });

    try {
      // Covers the race where the gateway became ready before we subscribed.
      const initial = await api.gatewayState();
      setGateway(initial);
      await ensureThread(initial);
    } catch (error) {
      push({ role: "error", text: `The agent is not reachable: ${error}` });
    }

    // On a fresh install, open the dashboard so the first-run wizard is seen.
    if (await api.needsSetup().catch(() => false)) {
      void api.openDashboard().catch(() => undefined);
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
    setSpeaking(null);
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
    setSpeaking(null);
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

  async function answerFill(request: BrowserApproval, approved: boolean) {
    // Drop it from the queue first: the decision is made, and a double-click must
    // not send two answers for one request.
    setFills((current) => current.filter((pending) => pending.id !== request.id));
    try {
      await api.answerBrowserFill(request.id, approved);
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
    <div class="widget" classList={{ "panel-closed": !panelOpen() }}>
      <Show when={panelOpen()}>
        <div class="bubble-panel">
          {/* Undecorated window: this strip is what the user drags. */}
          <header class="widget-header" data-tauri-drag-region>
            <span class="title" data-tauri-drag-region>
              IronClaw
            </span>
            <HealthBadge state={gateway()} />
            <button
              class="ghost"
              title="Dashboard"
              onClick={() => void api.openDashboard()}
            >
              ⋯
            </button>
            <button class="ghost" title="Hide chat" onClick={() => setPanelOpen(false)}>
              ▾
            </button>
          </header>

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

            {/*
              Sensitive-fill approval. Shows what will be typed and where — a
              consent prompt the user can't evaluate isn't consent. Default focus
              is Deny: the safe answer should be the easy one.
            */}
            <For each={fills()}>
              {(request) => (
                <div class="gate gate-fill">
                  <div class="gate-headline">The agent wants to type into a field</div>
                  <div class="gate-body">
                    <div class="fill-row">
                      <span class="fill-label">Field</span>
                      <span class="fill-value">{request.field}</span>
                    </div>
                    <div class="fill-row">
                      <span class="fill-label">On</span>
                      <span class="fill-value" classList={{ insecure: !request.secure }}>
                        {request.url}
                        {!request.secure ? " (not secure)" : ""}
                      </span>
                    </div>
                    <div class="fill-row">
                      <span class="fill-label">Text</span>
                      <span class="fill-value fill-text">{request.value}</span>
                    </div>
                    <Show when={request.reason}>
                      <div class="fill-reason">{request.reason}</div>
                    </Show>
                  </div>
                  <div class="gate-actions">
                    <button class="deny" onClick={() => void answerFill(request, false)}>
                      Don't type
                    </button>
                    <button class="approve" onClick={() => void answerFill(request, true)}>
                      Type it
                    </button>
                  </div>
                </div>
              )}
            </For>
          </div>

          <form class="composer" onSubmit={send}>
            <input
              type="text"
              placeholder={busy() ? "Working…" : "Ask something"}
              value={draft()}
              disabled={busy() || gateway().state !== "ready"}
              onInput={(event) => setDraft(event.currentTarget.value)}
              onFocus={() =>
                void api.setCharacterSignals({ listening: true }).catch(() => undefined)
              }
              onBlur={() =>
                void api.setCharacterSignals({ listening: false }).catch(() => undefined)
              }
            />
            <Show
              when={busy()}
              fallback={
                <button
                  type="submit"
                  disabled={!draft().trim() || gateway().state !== "ready"}
                >
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
      </Show>

      <CharacterView
        panelOpen={panelOpen}
        onHeadTap={() => setPanelOpen((open) => !open)}
      />
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
 * The character. Owns a {@link CharacterRenderer}; drives it from
 * `character://state` (reading the current state on mount to cover the race
 * where it changed before we subscribed), from `cursor://pos` for eye tracking,
 * and from `character://active` for the fullscreen pause. It also feeds the
 * click-through mask: the panel's rect plus the character's silhouette, pushed
 * whenever either can have changed.
 */
function CharacterView(props: { panelOpen: () => boolean; onHeadTap: () => void }) {
  let container: HTMLDivElement | undefined;

  // The voice loop's state, for the mic-live indicator. `null` when voice is off.
  const [voiceState, setVoiceState] = createSignal<VoiceState | null>(null);

  // Set once the renderer is mounted. The effect lives in the component body
  // (an async onMount continuation has no Solid owner), and re-pushes the mask
  // when the panel toggles — its rect just appeared or vanished.
  const [maskPusher, setMaskPusher] = createSignal<(() => void) | null>(null);
  createEffect(
    on(
      () => [props.panelOpen(), maskPusher()] as const,
      ([, push]) => {
        if (push) setTimeout(push, 60);
      },
      { defer: true },
    ),
  );

  onMount(async () => {
    if (!container) return;

    // Try the configured Live2D/sprite character; on any failure — missing
    // Core, a bad asset path, an unreadable config — fall back to the
    // placeholder so the character still reacts and the log carries the reason.
    let renderer: CharacterRenderer;
    try {
      const { config_url } = await api.characterSettings();
      const config = await loadCharacterConfig(config_url);
      renderer = createRenderer(config);
      await renderer.mount(container);
    } catch (error) {
      const detail = `character: falling back to the placeholder (Live2DCubismCore ${
        typeof (window as { Live2DCubismCore?: unknown }).Live2DCubismCore
      }) — ${String(error)}`;
      console.error(detail, error);
      renderer = createRenderer(PLACEHOLDER_CONFIG);
      await renderer.mount(container);
    }

    const unlistenState = await onCharacterState((state) => renderer.setState(state));
    renderer.setState(await api.characterState());
    const unlistenCursor = await onCursorPos((pos) => renderer.focus?.(pos.x, pos.y));
    const unlistenActive = await onCharacterActive(
      (active) => renderer.setActive?.(active),
    );
    // Real TTS amplitude → the character's mouth (replaces the Phase 3 test tone).
    const unlistenAmplitude = await onVoiceAmplitude(
      (level) => renderer.setMouthOpen?.(level),
    );
    // Voice-loop state → the mic-live indicator.
    const unlistenVoice = await onVoiceState((state) => setVoiceState(state));

    // Click head = summon/dismiss the chat panel; drag body = move the window.
    const onPointerDown = (event: PointerEvent) => {
      if (event.button !== 0) return;
      const hit = renderer.hitAt?.(event.clientX, event.clientY);
      if (hit === "head") props.onHeadTap();
      if (hit === "body") void api.startDragging().catch(() => undefined);
    };
    container.addEventListener("pointerdown", onPointerDown);

    // The click-through mask. Refreshed on a slow tick (the silhouette drifts
    // with idle motion), and immediately when the panel toggles.
    const pushMask = () => {
      const profile = renderer.hitProfile?.(MASK_CELL) ?? null;
      const solids = Array.from(
        document.querySelectorAll<HTMLElement>(".bubble-panel"),
      );
      void api.setHitMask(buildHitMask(profile, solids)).catch(() => undefined);
    };
    pushMask();
    const maskTimer = setInterval(pushMask, 700);
    setMaskPusher(() => pushMask);

    onCleanup(() => {
      clearInterval(maskTimer);
      container?.removeEventListener("pointerdown", onPointerDown);
      unlistenState();
      unlistenCursor();
      unlistenActive();
      unlistenAmplitude();
      unlistenVoice();
      renderer.destroy();
    });
  });

  // The mic-live indicator: a small dot on the character while the loop is active.
  // Hidden when voice is off, idle, or muted; it lights up while listening and
  // pulses through the rest of a spoken turn.
  const micLabel = (): string | null => {
    switch (voiceState()) {
      case "listening":
        return "listening";
      case "transcribing":
      case "sending":
        return "thinking";
      case "speaking":
        return "speaking";
      default:
        return null; // idle / muted / off
    }
  };

  return (
    <div class="character" ref={container}>
      <Show when={micLabel()}>
        {(label) => (
          <div class="mic-indicator" data-voice={voiceState() ?? ""} title={`Voice: ${label()}`}>
            <span class="mic-dot" />
            <span class="mic-label">{label()}</span>
          </div>
        )}
      </Show>
    </div>
  );
}

const root = document.getElementById("root");
if (root) render(() => <App />, root);
