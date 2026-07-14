/**
 * One conversation, shared by two windows.
 *
 * The widget and the dashboard are separate webviews showing the *same* thread:
 * the character speaks the reply, the dashboard holds the transcript and the
 * composer. So the chat engine lives here rather than inside either window, and
 * the thread itself is owned by Rust (`api.currentThread()` creates it once and
 * everyone else joins) — if each webview created its own, the user would type into
 * one and watch the other.
 *
 * Two facts from the gateway shape everything below:
 *
 * - **The event stream carries no assistant text.** It carries `run_status` and
 *   nothing else, on a 1-second poll. So a reply is not streamed; the run goes
 *   terminal and *then* the text is fetched from the timeline. There is no token
 *   streaming to render, and `speaking` starts when the reply lands, not at a
 *   first token. See `docs/desktop/chat-rendering.md`.
 * - **A gate parks the run until it is answered.** Approval prompts are not
 *   advisory, so they must reach the user even when the dashboard is closed —
 *   which is why the widget renders them too.
 */
import { createSignal, onCleanup } from "solid-js";

import {
  api,
  onBrowserApproval,
  onChatEvent,
  onGatewayState,
  onThreadChanged,
  onVoiceTranscript,
  TERMINAL_PHASES,
  type BrowserApproval,
  type ChatEvent,
  type GatewayState,
  type Message,
  type RunPhase,
} from "./api";

/** A line in the transcript. */
export interface Bubble {
  role: "user" | "assistant" | "error";
  text: string;
  /** Wall-clock, for the dashboard's transcript. */
  at: number;
}

/** A pending tool approval. The run is parked until it is answered. */
export interface Gate {
  runId: string;
  gateRef: string;
  headline: string;
  body: string;
}

export type Chat = ReturnType<typeof createChat>;

/**
 * The shared conversation. Call once per window; both windows drive the same
 * underlying thread.
 */
export function createChat(options: { speaks?: boolean } = {}) {
  const [threadId, setThreadId] = createSignal<string | null>(null);
  const [bubbles, setBubbles] = createSignal<Bubble[]>([]);
  const [activeRun, setActiveRun] = createSignal<string | null>(null);
  const [phase, setPhase] = createSignal<RunPhase | null>(null);
  /** A Stop is in flight. The button must not fire twice, and the UI says so. */
  const [stopping, setStopping] = createSignal(false);
  const [activity, setActivity] = createSignal<string | null>(null);
  const [gate, setGate] = createSignal<Gate | null>(null);
  const [gateway, setGateway] = createSignal<GatewayState>({ state: "starting" });
  /** The newest assistant reply — what the character says out loud. */
  const [lastReply, setLastReply] = createSignal<string | null>(null);
  /** The last thing the microphone heard. `""` means it heard nothing. */
  const [heard, setHeard] = createSignal<string | null>(null);
  /**
   * Sensitive-fill approvals queue. A page may ask for several in a row and each
   * needs its own explicit answer, so they are never collapsed into one.
   */
  const [fills, setFills] = createSignal<BrowserApproval[]>([]);
  /** Runs whose reply has already been collected. See `handleChatEvent`. */
  const collected = new Set<string>();
  /**
   * Whether this turn began with the user's voice. A *spoken* turn is already read
   * aloud by the voice pipeline itself, so asking it to speak again would say the
   * answer twice. A *typed* turn has no such path — which is why the app could never
   * talk back to a user who typed, whatever their reply mode said.
   */
  let spokenTurn = false;

  const push = (bubble: Omit<Bubble, "at">) =>
    setBubbles((current) => [...current, { ...bubble, at: Date.now() }]);

  /**
   * `speaking` drives the character's mouth and face. With TTS on, real playback
   * amplitude takes over; with it off, the window is a reading-time estimate, so
   * the character does not keep talking after the user has finished reading.
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
        setLastReply(reply.content);
        setSpeaking(reply.content);
        // One window speaks (the widget), and only for a turn the pipeline is not
        // already speaking. Rust decides *whether* to, from the persisted reply mode.
        if (options.speaks && !spokenTurn) {
          void api.speakReply(reply.content).catch(() => undefined);
        }
      }
      spokenTurn = false;
    } catch (error) {
      push({ role: "error", text: `Could not load the reply: ${error}` });
    }
  }

  function handleChatEvent(event: ChatEvent) {
    switch (event.kind) {
      case "run_status": {
        // The gateway re-sends a run's terminal status (it arrives on both the
        // projection snapshot and the update), and the projection is a 1-second
        // poll — so a run finishes *more than once* on the wire. Collect each run
        // exactly once, or the same reply is pushed into the transcript twice and
        // the character says it twice.
        if (collected.has(event.run_id)) return;

        // **Adopt a run we did not start.** The two windows are two views of one
        // conversation: the user types in the dashboard, and the *character* has to
        // speak the answer. If each window only tracked runs it started itself, the
        // widget would sit mute through every message typed in the dashboard — and
        // the dashboard would miss every reply to something said out loud.
        //
        // Safe because the pump is per-thread and Rust replaces it when the thread
        // changes, so every event here belongs to the conversation we are showing.
        if (activeRun() === null) setActiveRun(event.run_id);
        if (event.run_id !== activeRun()) return;
        setPhase(event.phase);
        if (TERMINAL_PHASES.has(event.phase)) {
          collected.add(event.run_id);
          const failed = event.phase === "failed" || event.phase === "killed";
          void collectReply(failed ? (event.failure_summary ?? "The turn failed.") : null);
        }
        break;
      }
      case "gate":
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
        break;
    }
  }

  /**
   * The gateway takes ~500 ms to boot (much longer on a first run, which migrates
   * the database and installs bundled skills), so joining the thread on mount
   * would fail with "still starting" and never retry. Join on the first `ready` we
   * observe, whether it arrives as an event or as the state we read below.
   */
  let joining = false;
  async function ensureThread(state: GatewayState) {
    if (state.state !== "ready" || threadId() || joining) return;
    joining = true;
    try {
      setThreadId(await api.currentThread());
    } catch (error) {
      push({ role: "error", text: `Could not start a conversation: ${error}` });
    } finally {
      joining = false;
    }
  }

  /** Wire up the event listeners. Returns a teardown. */
  async function start(): Promise<() => void> {
    const cleanups: (() => void)[] = [];

    cleanups.push(
      await onGatewayState((state) => {
        setGateway(state);
        void ensureThread(state);
      }),
    );
    cleanups.push(await onChatEvent(handleChatEvent));
    cleanups.push(await onBrowserApproval((request) => setFills((q) => [...q, request])));
    // A spoken question never appeared anywhere: voice sent it straight to the
    // gateway, so the transcript held the reply but not the question, and the user
    // could not tell a misheard word from a broken microphone. Now what was heard is
    // shown — and an empty transcript says so out loud.
    cleanups.push(
      await onVoiceTranscript((text) => {
        const heard = text.trim();
        spokenTurn = true;
        if (heard) {
          push({ role: "user", text: heard });
          setHeard(heard);
        } else {
          push({ role: "error", text: "I listened, but heard nothing." });
          setHeard("");
        }
      }),
    );
    // The other window started a new session; follow it rather than bubbling
    // replies from a conversation the user has left.
    cleanups.push(
      await onThreadChanged((id) => {
        setThreadId(id);
        setBubbles([]);
        setLastReply(null);
        setActiveRun(null);
        setPhase(null);
        setGate(null);
      }),
    );

    try {
      // Covers the race where the gateway became ready before we subscribed.
      const initial = await api.gatewayState();
      setGateway(initial);
      await ensureThread(initial);
    } catch (error) {
      push({ role: "error", text: `The agent is not reachable: ${error}` });
    }

    return () => {
      cleanups.forEach((fn) => fn());
      clearTimeout(speakingTimer);
    };
  }

  async function send(text: string) {
    const trimmed = text.trim();
    const id = threadId();
    if (!trimmed || !id || activeRun()) return;

    push({ role: "user", text: trimmed });
    setPhase("queued");
    setLastReply(null);
    setSpeaking(null);
    try {
      const { run_id } = await api.sendMessage(id, trimmed);
      setActiveRun(run_id);
    } catch (error) {
      setPhase(null);
      push({ role: "error", text: `Could not send: ${error}` });
    }
  }

  async function stop() {
    const id = threadId();
    const run = activeRun();
    if (!id || !run || stopping()) return;
    setSpeaking(null);
    setStopping(true);
    try {
      // Two ways to have nothing left to stop, neither of them a failure:
      // `already_terminal` — the reply landed while the click was in the air
      // (the common race); `unknown` — the gateway has never heard of this run
      // (a stale id we were holding). Both collect the reply and move on.
      //
      // Note what this does NOT do: cancelling does not abort the model's
      // in-flight generation. A local llama-server keeps generating to
      // completion — Stop means "stop showing me this", not "stop computing it".
      const { already_terminal, unknown } = await api.cancelRun(id, run);
      if (already_terminal || unknown) await collectReply(null);
    } catch (error) {
      push({ role: "error", text: `Could not stop: ${error}` });
    } finally {
      setStopping(false);
    }
  }

  /** Start a fresh conversation in *both* windows. */
  async function newSession() {
    try {
      await api.newThread();
    } catch (error) {
      push({ role: "error", text: `Could not start a new session: ${error}` });
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
  const ready = () => gateway().state === "ready" && threadId() !== null;

  /** What the character is doing, in words. `null` when idle. */
  const statusLine = () => {
    const current = phase();
    // Say "stopping…" the moment the click lands, not when the gateway gets
    // round to admitting it: the cancel is a *request*, and the run can sit in
    // `running` for another poll or two before it turns over.
    if (stopping()) return "stopping";
    if (gate()) return "waiting for you";
    if (activity()) return `running ${activity()}`;
    if (!current || TERMINAL_PHASES.has(current)) return null;
    if (current === "queued") return "queued";
    if (current === "cancel_requested") return "stopping";
    if (current.startsWith("blocked")) return "waiting for you";
    return "thinking";
  };

  onCleanup(() => clearTimeout(speakingTimer));

  return {
    threadId,
    bubbles,
    gate,
    fills,
    gateway,
    lastReply,
    setLastReply,
    heard,
    phase,
    busy,
    stopping,
    ready,
    statusLine,
    start,
    send,
    stop,
    newSession,
    answerGate,
    answerFill,
  };
}
