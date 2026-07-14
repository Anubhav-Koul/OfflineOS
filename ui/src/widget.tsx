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
  onAmbientSuggestion,
  onSkillInstallResult,
  onCharacterActive,
  onCharacterState,
  onCursorPos,
  onProfileChanged,
  onVoiceAmplitude,
  onVoiceState,
  type GatewayState,
  type HitMask,
  type Profile,
  type Suggestion,
  type VoiceState,
} from "./api";
import { createChat } from "./chat";
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
 * The desktop companion window: **the character, and nothing else.**
 *
 * There is no frame, no panel, and no composer here. The window is transparent
 * and undecorated, and only two kinds of thing are ever drawn over it — speech
 * (a balloon anchored to the character's head) and an interruption (an approval
 * the parked run is waiting on). Both are transient. Everything durable —
 * the transcript, typing, uploads, settings, sessions, models, avatars — lives in
 * the dashboard, which the character's head opens.
 *
 * Clicking the head opens the dashboard; dragging the body moves the window;
 * everything outside the character and its balloon passes through to the desktop
 * (the hit mask below).
 */

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
  // The widget is the character: it is the one that talks.
  const chat = createChat({ speaks: true });
  const [profile, setProfile] = createSignal<Profile>({
    user_name: "",
    assistant_name: "",
    reply_mode: "read",
  });

  /**
   * What the character is currently saying. Cleared on a reading-time estimate so
   * the balloon does not hang over the desktop forever — the transcript in the
   * dashboard is the durable record; this is speech, and speech ends.
   */
  const [says, setSays] = createSignal<string | null>(null);
  let saysTimer: ReturnType<typeof setTimeout> | undefined;

  /**
   * What the character has volunteered and is waiting on an answer for. Only ever
   * one: the guardrail caps surfacings at a couple an hour, and a stack of unread
   * suggestions is a notification centre, which is the thing this is not.
   */
  const [suggestion, setSuggestion] = createSignal<Suggestion | null>(null);
  const answer = async (accepted: boolean) => {
    const pending = suggestion();
    if (!pending) return;
    setSuggestion(null);
    await api.respondSuggestion(pending.id, accepted).catch(() => undefined);
  };

  /** The one-line receipt after an approved skill draft installs (or fails). */
  const [installNote, setInstallNote] = createSignal<string | null>(null);
  let installNoteTimer: ReturnType<typeof setTimeout> | undefined;

  createEffect(() => {
    const reply = chat.lastReply();
    clearTimeout(saysTimer);
    // `hear` means the reply is spoken and not shown; the balloon would be a
    // second, redundant channel.
    if (!reply || profile().reply_mode === "hear") {
      setSays(null);
      return;
    }
    setSays(reply);
    const readingMs = Math.min(4000 + reply.length * 45, 20_000);
    saysTimer = setTimeout(() => setSays(null), readingMs);
  });

  onMount(() => {
    const cleanups: (() => void)[] = [];
    onCleanup(() => {
      cleanups.forEach((fn) => fn());
      clearTimeout(saysTimer);
    });

    void (async () => {
      cleanups.push(await chat.start());
      cleanups.push(await onProfileChanged(setProfile));
      cleanups.push(await onAmbientSuggestion(setSuggestion));
      cleanups.push(
        await onSkillInstallResult((result) => {
          setInstallNote(
            result.ok
              ? `Learned “${result.name}”. I can use it from the next task.`
              : `I couldn't save that skill: ${result.error}`,
          );
          clearTimeout(installNoteTimer);
          installNoteTimer = setTimeout(() => setInstallNote(null), 8000);
        }),
      );
      try {
        setProfile(await api.profile());
      } catch {
        /* silent-ok: an unnamed assistant still works */
      }
      // On a fresh install, open the dashboard so the first-run wizard is seen.
      if (await api.needsSetup().catch(() => false)) {
        void api.openDashboard().catch(() => undefined);
      }
    })();
  });

  // A parked run needs an answer, and the dashboard may be closed — so approvals
  // live on the character too. They are the one thing that interrupts.
  const interrupting = () => chat.gate() !== null || chat.fills().length > 0;

  return (
    <div class="widget">
      {/*
        The character *is* the window. There is no panel, no frame, and no
        composer here: typing, history, and settings live in the dashboard, which
        the character's head opens. Everything the widget shows is speech or an
        interruption, and both are anchored to the character.
      */}
      <div class="stage">
        {/*
          The character speaking first (Phase 7a). It is not a notification: it is
          a question, with two answers, and "Not now" is recorded so the same
          source stays quiet for a while. A gate still outranks it — that one is
          blocking a run the user asked for.

          A skill draft (Phase 7b) wears the red consent style, not the blue offer:
          Accept *installs* — it changes what the agent can do from now on — so the
          card shows the full text, and No is the default answer.
        */}
        <Show when={!interrupting() && suggestion()}>
          {(pending) => (
            <div
              class={`ask solid ${
                pending().kind === "skill_draft" ? "ask-install" : "ask-suggest"
              }`}
            >
              <div class="ask-headline">{pending().headline}</div>
              <Show
                when={pending().kind === "skill_draft"}
                fallback={<div class="ask-body">{pending().body}</div>}
              >
                <pre class="ask-skill-text">{pending().body}</pre>
                <div class="ask-warning">
                  Installing lets the agent use this in future tasks. Review it
                  before saying yes.
                </div>
              </Show>
              <div class="ask-actions">
                <button
                  class="deny"
                  autofocus={pending().kind === "skill_draft"}
                  onClick={() => void answer(false)}
                >
                  {pending().kind === "skill_draft" ? "No" : "Not now"}
                </button>
                <button class="approve" onClick={() => void answer(true)}>
                  {pending().kind === "skill_draft"
                    ? "Install skill"
                    : pending().thread_id
                      ? "Show me"
                      : "Thanks"}
                </button>
              </div>
            </div>
          )}
        </Show>

        {/* The one-line receipt after an approved draft installs (or fails to). */}
        <Show when={!interrupting() && !suggestion() && installNote()}>
          {(note) => (
            <div class="say solid">
              <div class="say-cloud">{note()}</div>
              <div class="say-tail" />
            </div>
          )}
        </Show>

        <Show when={says() && !interrupting() && !suggestion() && !installNote()}>
          {(text) => (
            <div class="say solid">
              <div class="say-cloud">{text()}</div>
              <div class="say-tail" />
            </div>
          )}
        </Show>

        <Show when={!says() && chat.statusLine() && !interrupting() && !suggestion()}>
          {(line) => (
            <div class="say say-thinking solid">
              <div class="say-cloud">{line()}</div>
              <div class="say-tail" />
            </div>
          )}
        </Show>

        <Show when={chat.gateway().state !== "ready" && !interrupting() && !suggestion()}>
          <div class="say say-status solid">
            <div class="say-cloud">
              <HealthBadge state={chat.gateway()} />
            </div>
            <div class="say-tail" />
          </div>
        </Show>

        <Show when={chat.gate()}>
          {(pending) => (
            <div class="ask solid">
              <div class="ask-headline">{pending().headline}</div>
              <pre class="ask-body">{pending().body}</pre>
              <div class="ask-actions">
                <button class="approve" onClick={() => void chat.answerGate(true)}>
                  Allow once
                </button>
                <button class="deny" onClick={() => void chat.answerGate(false)}>
                  Deny
                </button>
              </div>
            </div>
          )}
        </Show>

        {/*
          Sensitive-fill approval. Shows what will be typed and where — a consent
          prompt the user can't evaluate isn't consent. Deny is the easy answer.
        */}
        <For each={chat.fills()}>
          {(request) => (
            <div class="ask ask-fill solid">
              <div class="ask-headline">The agent wants to type into a field</div>
              <div class="ask-body">
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
              <div class="ask-actions">
                <button class="deny" onClick={() => void chat.answerFill(request, false)}>
                  Don't type
                </button>
                <button class="approve" onClick={() => void chat.answerFill(request, true)}>
                  Type it
                </button>
              </div>
            </div>
          )}
        </For>
      </div>

      <CharacterView
        speaking={() => says() !== null || interrupting() || suggestion() !== null}
        onHeadTap={() => void api.openDashboard().catch(() => undefined)}
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
function CharacterView(props: { speaking: () => boolean; onHeadTap: () => void }) {
  let container: HTMLDivElement | undefined;

  // The voice loop's state, for the mic-live indicator. `null` when voice is off.
  const [voiceState, setVoiceState] = createSignal<VoiceState | null>(null);

  // Set once the renderer is mounted. The effect lives in the component body
  // (an async onMount continuation has no Solid owner), and re-pushes the mask
  // whenever the character starts or stops speaking — a balloon or an approval
  // card just appeared or vanished, and those are the only solid things here.
  const [maskPusher, setMaskPusher] = createSignal<(() => void) | null>(null);
  createEffect(
    on(
      () => [props.speaking(), maskPusher()] as const,
      ([, push]) => {
        if (push) setTimeout(push, 60);
      },
      { defer: true },
    ),
  );

  onMount(() => {
    if (!container) return;

    // onCleanup must register SYNCHRONOUSLY (after an `await` the Solid owner is
    // gone and it silently never registers); the async work below appends its
    // teardown steps to this list as it creates things.
    const cleanups: (() => void)[] = [];
    onCleanup(() => cleanups.forEach((fn) => fn()));

    void (async () => {
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
      cleanups.push(() => renderer.destroy());

      cleanups.push(await onCharacterState((state) => renderer.setState(state)));
      renderer.setState(await api.characterState());
      cleanups.push(await onCursorPos((pos) => renderer.focus?.(pos.x, pos.y)));
      cleanups.push(
        await onCharacterActive((active) => renderer.setActive?.(active)),
      );
      // Real TTS amplitude → the character's mouth (replaces the Phase 3 test
      // tone).
      cleanups.push(
        await onVoiceAmplitude((level) => renderer.setMouthOpen?.(level)),
      );
      // Voice-loop state → the mic-live indicator.
      cleanups.push(await onVoiceState((state) => setVoiceState(state)));

      // Click head = open the dashboard (the transcript, settings, everything);
      // drag body = move the window.
      const onPointerDown = (event: PointerEvent) => {
        if (event.button !== 0) return;
        const hit = renderer.hitAt?.(event.clientX, event.clientY);
        if (hit === "head") props.onHeadTap();
        if (hit === "body") void api.startDragging().catch(() => undefined);
      };
      container.addEventListener("pointerdown", onPointerDown);
      cleanups.push(() => container?.removeEventListener("pointerdown", onPointerDown));

      // The click-through mask. Refreshed on a slow tick (the silhouette drifts
      // with idle motion), and immediately when the panel toggles.
      const pushMask = () => {
        const profile = renderer.hitProfile?.(MASK_CELL) ?? null;
        // Everything clickable in this window carries `.solid`: the speech
        // balloon and the approval cards. Anything else is desktop.
        const solids = Array.from(document.querySelectorAll<HTMLElement>(".solid"));
        void api.setHitMask(buildHitMask(profile, solids)).catch(() => undefined);
      };
      pushMask();
      const maskTimer = setInterval(pushMask, 700);
      cleanups.push(() => clearInterval(maskTimer));
      setMaskPusher(() => pushMask);
    })();
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
