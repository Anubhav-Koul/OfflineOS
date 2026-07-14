/* @refresh reload */
import { render } from "solid-js/web";
import { createEffect, createSignal, For, onCleanup, onMount, Show } from "solid-js";

import {
  api,
  onGatewayState,
  onModelEvent,
  onVoiceState,
  onVoiceTranscript,
  type AmbientStatus,
  type Automation,
  type GatewayState,
  type InstalledModel,
  type LocalModel,
  type Provider,
  type ProviderSelection,
  type ProviderSettings,
  type RecommendedModel,
  type ReplyMode,
  type Thread,
  type VoiceState,
} from "./api";
import { createChat } from "./chat";
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

/** An active download's live state. */
interface ActiveDownload {
  id: string;
  downloaded: number;
  total: number | null;
  fraction: number | null;
}

/** Bytes as a compact GiB/MiB string. */
function formatSize(bytes: number): string {
  const gib = bytes / 1024 ** 3;
  if (gib >= 1) return `${gib.toFixed(1)} GiB`;
  return `${Math.round(bytes / 1024 ** 2)} MiB`;
}

/**
 * Download and manage local GGUF models.
 *
 * Loads on its own — it reads the model store on disk, not the gateway. A
 * download streams progress over `model://event`; only one runs at a time.
 */
function ModelsPanel() {
  const [recommended, setRecommended] = createSignal<RecommendedModel[]>([]);
  const installed = createPanelData<InstalledModel>(api.installedModels);
  const [active, setActive] = createSignal<ActiveDownload | null>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  const [repo, setRepo] = createSignal("");
  const [file, setFile] = createSignal("");

  onMount(async () => {
    try {
      setRecommended(await api.recommendedModels());
    } catch (reason) {
      setNotice(String(reason));
    }
    void installed.refresh();

    const unlisten = await onModelEvent((event) => {
      if (event.kind === "progress") {
        setActive({
          id: event.id,
          downloaded: event.downloaded,
          total: event.total,
          fraction: event.fraction,
        });
        return;
      }
      // finished
      setActive(null);
      if (event.cancelled) setNotice("Download cancelled.");
      else if (!event.ok) setNotice(event.error ?? "Download failed.");
      else setNotice(null);
      void installed.refresh();
    });
    onCleanup(unlisten);
  });

  const isInstalled = (id: string) => installed.rows().some((model) => model.id === id);

  const download = async (repoName: string, fileName: string) => {
    setNotice(null);
    // Show the transfer immediately; the id the backend derives is the file
    // stem, and progress events refine the rest.
    setActive({ id: fileName.replace(/\.gguf$/, ""), downloaded: 0, total: null, fraction: null });
    try {
      await api.downloadModel(repoName, fileName);
    } catch (reason) {
      setNotice(String(reason));
      setActive(null);
    }
  };

  const cancel = async () => {
    try {
      await api.cancelDownload();
    } catch (reason) {
      setNotice(String(reason));
    }
  };

  const remove = async (id: string) => {
    setNotice(null);
    try {
      await api.removeModel(id);
      await installed.refresh();
    } catch (reason) {
      setNotice(String(reason));
    }
  };

  const downloadCustom = () => {
    const fileName = file().trim();
    const repoName = repo().trim();
    if (!repoName || !fileName) return;
    void download(repoName, fileName);
    setRepo("");
    setFile("");
  };

  return (
    <section>
      <div class="panel-head">
        <h2>Models</h2>
        <button class="ghost" disabled={installed.loading()} onClick={() => void installed.refresh()}>
          Refresh
        </button>
      </div>

      <Show when={active()}>
        {(dl) => (
          <div class="download">
            <div class="download-head">
              <span class="row-title">Downloading {dl().id}</span>
              <button class="ghost danger" onClick={() => void cancel()}>
                Cancel
              </button>
            </div>
            <div class="progress">
              <div
                class="progress-bar"
                style={{ width: dl().fraction != null ? `${dl().fraction! * 100}%` : "100%" }}
                classList={{ indeterminate: dl().fraction == null }}
              />
            </div>
            <span class="row-meta">
              {formatSize(dl().downloaded)}
              {dl().total != null ? ` / ${formatSize(dl().total!)}` : ""}
            </span>
          </div>
        )}
      </Show>

      <Show when={notice()}>
        <p class="reason-inline">{notice()}</p>
      </Show>

      <h3 class="subhead">Suggested</h3>
      <ul class="rows">
        <For each={recommended()}>
          {(model) => (
            <li class="row model-card">
              <div class="model-card-main">
                <span class="row-title">{model.name}</span>
                <span class="row-meta">
                  {model.params} · {model.quant} · ~{model.approx_gib} GiB
                </span>
                <p class="provider-desc">{model.note}</p>
              </div>
              <button
                disabled={active() != null || isInstalled(model.id)}
                onClick={() => void download(model.repo, model.file)}
              >
                {isInstalled(model.id) ? "Installed" : "Download"}
              </button>
            </li>
          )}
        </For>
      </ul>

      <h3 class="subhead">Custom (any HuggingFace GGUF)</h3>
      <div class="key-row custom-download">
        <input
          type="text"
          placeholder="owner/repo"
          value={repo()}
          onInput={(event) => setRepo(event.currentTarget.value)}
        />
        <input
          type="text"
          placeholder="file.gguf"
          value={file()}
          onInput={(event) => setFile(event.currentTarget.value)}
        />
        <button disabled={active() != null || !repo().trim() || !file().trim()} onClick={downloadCustom}>
          Download
        </button>
      </div>

      <h3 class="subhead">Installed</h3>
      <Show
        when={!installed.error()}
        fallback={<p class="reason-inline">{installed.error()}</p>}
      >
        <Show
          when={installed.rows().length > 0}
          fallback={<p class="muted">No models downloaded yet.</p>}
        >
          <ul class="rows">
            <For each={installed.rows()}>
              {(model) => (
                <li class="row">
                  <div class="model-card-main">
                    <span class="row-title">{model.id}</span>
                    <span class="row-meta">
                      {model.size_mb >= 1024
                        ? `${(model.size_mb / 1024).toFixed(1)} GiB`
                        : `${model.size_mb} MiB`}
                    </span>
                    <Show when={model.suspect}>
                      {(reason) => <p class="reason-inline">{reason()}</p>}
                    </Show>
                  </div>
                  <button class="ghost danger" onClick={() => void remove(model.id)}>
                    Remove
                  </button>
                </li>
              )}
            </For>
          </ul>
        </Show>
      </Show>
    </section>
  );
}

/** One bundled character, from `/characters/manifest.json`. */
interface CharacterEntry {
  id: string;
  name: string;
  note: string;
}

/**
 * The character picker (Phase 3: character choice is a settings toggle).
 *
 * The available list comes from a static manifest bundled with the assets —
 * the folders are embedded in the binary, so there is nothing to scan at
 * runtime. Applying reloads the widget window, which remounts its renderer.
 */
function CharacterPanel() {
  const [available, setAvailable] = createSignal<CharacterEntry[]>([]);
  const [active, setActive] = createSignal<string | null>(null);
  const [chosen, setChosen] = createSignal<string | null>(null);
  const [notice, setNotice] = createSignal<string | null>(null);

  onMount(async () => {
    try {
      const response = await fetch("/characters/manifest.json");
      if (response.ok) {
        const manifest = (await response.json()) as { characters: CharacterEntry[] };
        setAvailable(manifest.characters);
      }
      const settings = await api.characterSettings();
      setActive(settings.active);
      setChosen(settings.active);
    } catch (reason) {
      setNotice(String(reason));
    }
  });

  const apply = async () => {
    const id = chosen();
    if (!id) return;
    setNotice(null);
    try {
      await api.setCharacter(id);
      setActive(id);
    } catch (reason) {
      setNotice(String(reason));
    }
  };

  return (
    <section>
      <h2>Character</h2>
      <p class="muted small">
        A character is an asset folder under <code>characters/</code>; switching
        reloads the widget.
      </p>
      <For each={available()}>
        {(entry) => (
          <label class="provider-option">
            <input
              type="radio"
              name="character"
              checked={chosen() === entry.id}
              onChange={() => setChosen(entry.id)}
            />
            <span class="provider-name">{entry.name}</span>
            <span class="row-meta">{entry.note}</span>
          </label>
        )}
      </For>
      <Show when={notice()}>
        <p class="reason-inline">{notice()}</p>
      </Show>
      <div class="apply-row">
        <button disabled={!chosen() || chosen() === active()} onClick={() => void apply()}>
          Apply
        </button>
      </div>
    </section>
  );
}

/** Whether two selections name the same provider (model override aside). */
function selectionEquals(a: ProviderSelection, b: ProviderSelection): boolean {
  if (a.kind !== b.kind) return false;
  if (a.kind === "cloud" && b.kind === "cloud") return a.id === b.id;
  return true;
}

/**
 * The active-provider switch and API-key manager.
 *
 * Loads on its own — it reads the settings file and the credential store, not
 * the gateway, so it works before the gateway is ready. Applying a change
 * restarts the gateway on the backend, which reloads this page.
 */
function ProviderPanel() {
  const data = createValueData<ProviderSettings>(api.providerSettings);
  const [chosen, setChosen] = createSignal<ProviderSelection>({ kind: "local" });
  const [drafts, setDrafts] = createSignal<Record<string, string>>({});
  const [applying, setApplying] = createSignal(false);
  const [notice, setNotice] = createSignal<string | null>(null);

  const reload = async () => {
    await data.refresh();
    const current = data.value();
    if (current) setChosen(current.active);
  };
  onMount(reload);

  const draft = (id: string) => drafts()[id] ?? "";
  const setDraft = (id: string, value: string) => setDrafts((all) => ({ ...all, [id]: value }));

  const saveKey = async (id: string) => {
    const key = draft(id).trim();
    if (!key) return;
    setNotice(null);
    try {
      await api.setProviderKey(id, key);
      setDraft(id, "");
      await data.refresh();
    } catch (reason) {
      setNotice(String(reason));
    }
  };

  const clearKey = async (id: string) => {
    setNotice(null);
    try {
      await api.clearProviderKey(id);
      await data.refresh();
    } catch (reason) {
      setNotice(String(reason));
    }
  };

  // A cloud provider with no stored key would start the gateway unconfigured.
  const chosenNeedsKey = (): boolean => {
    const pick = chosen();
    if (pick.kind !== "cloud") return false;
    const provider = data.value()?.providers.find((p) => p.id === pick.id);
    return !provider?.has_key;
  };

  const canApply = (): boolean => {
    const current = data.value();
    if (!current || applying()) return false;
    if (selectionEquals(chosen(), current.active)) return false;
    return !chosenNeedsKey();
  };

  const apply = async () => {
    setApplying(true);
    setNotice(null);
    try {
      // Restarts the gateway and reloads this page; the promise may not settle
      // before the reload takes over.
      await api.applyProvider(chosen());
    } catch (reason) {
      setNotice(String(reason));
      setApplying(false);
    }
  };

  return (
    <section>
      <div class="panel-head">
        <h2>Provider</h2>
        <button class="ghost" disabled={data.loading()} onClick={() => void reload()}>
          Refresh
        </button>
      </div>
      <p class="muted small">
        One provider is active at a time. Switching restarts the agent.
      </p>

      <Show when={!data.error()} fallback={<p class="reason-inline">{data.error()}</p>}>
        <Show when={data.value()} fallback={<p class="muted">Loading…</p>}>
          {(settings) => (
            <>
              <label class="provider-option">
                <input
                  type="radio"
                  name="provider"
                  checked={chosen().kind === "local"}
                  onChange={() => setChosen({ kind: "local" })}
                />
                <span class="provider-name">Local model</span>
                <span class="row-meta">bundled llama.cpp · no key needed</span>
              </label>

              <For each={settings().providers}>
                {(provider) => (
                  <ProviderRow
                    provider={provider}
                    active={
                      chosen().kind === "cloud" &&
                      (chosen() as { id: string }).id === provider.id
                    }
                    draft={draft(provider.id)}
                    onSelect={() => setChosen({ kind: "cloud", id: provider.id })}
                    onDraft={(value) => setDraft(provider.id, value)}
                    onSave={() => void saveKey(provider.id)}
                    onClear={() => void clearKey(provider.id)}
                  />
                )}
              </For>

              <Show when={notice()}>
                <p class="reason-inline">{notice()}</p>
              </Show>
              <Show when={chosenNeedsKey()}>
                <p class="muted small">Add an API key for this provider before applying.</p>
              </Show>

              <div class="apply-row">
                <button disabled={!canApply()} onClick={() => void apply()}>
                  {applying() ? "Applying…" : "Apply & restart"}
                </button>
              </div>
            </>
          )}
        </Show>
      </Show>
    </section>
  );
}

/** One selectable cloud provider with its API-key controls. */
function ProviderRow(props: {
  provider: Provider;
  active: boolean;
  draft: string;
  onSelect: () => void;
  onDraft: (value: string) => void;
  onSave: () => void;
  onClear: () => void;
}) {
  return (
    <div class="provider-option column">
      <label class="provider-head">
        <input type="radio" name="provider" checked={props.active} onChange={props.onSelect} />
        <span class="provider-name">{props.provider.id}</span>
        <Show when={props.provider.has_key} fallback={<span class="row-meta">no key</span>}>
          <span class="badge ready">key set</span>
        </Show>
      </label>
      <p class="provider-desc">{props.provider.description}</p>
      <div class="key-row">
        <input
          type="password"
          placeholder={props.provider.has_key ? "Replace API key" : "Paste API key"}
          value={props.draft}
          onInput={(event) => props.onDraft(event.currentTarget.value)}
        />
        <button class="ghost" disabled={!props.draft.trim()} onClick={props.onSave}>
          Save
        </button>
        <Show when={props.provider.has_key}>
          <button class="ghost danger" onClick={props.onClear}>
            Clear
          </button>
        </Show>
      </div>
    </div>
  );
}

/**
 * The first-run wizard: a welcome overlay shown until setup is marked complete. It
 * orients the user toward the model/provider panels already on the dashboard (rather
 * than duplicating them), offers to enable voice, and on "Finish" persists the flag
 * so it never shows again. Storage needs no step — the gateway initialises its
 * libSQL store on boot (no Postgres).
 */
function SetupWizard(props: { onDone: () => void }) {
  const [step, setStep] = createSignal(0);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  // Step 1 — the model. The download is *started* here and keeps streaming while
  // the user answers everything else, so the wizard and the bytes finish together
  // instead of ending on a progress bar.
  const [models, setModels] = createSignal<RecommendedModel[]>([]);
  const [installed, setInstalled] = createSignal<InstalledModel[]>([]);
  const [downloading, setDownloading] = createSignal<string | null>(null);
  const [fraction, setFraction] = createSignal<number | null>(null);
  const [downloadDone, setDownloadDone] = createSignal(false);

  // Step 2 — the names.
  const [userName, setUserName] = createSignal("");
  const [assistantName, setAssistantName] = createSignal("");
  const [replyMode, setReplyMode] = createSignal<ReplyMode>("both");

  // Step 3 — the wake word.
  const [voiceOn, setVoiceOn] = createSignal(true);

  onMount(async () => {
    const cleanups: (() => void)[] = [];
    onCleanup(() => cleanups.forEach((fn) => fn()));
    try {
      setModels(await api.recommendedModels());
      setInstalled(await api.installedModels());
      // A machine that already has a model does not need to download one.
      if (installed().length > 0) setDownloadDone(true);
    } catch (problem) {
      setError(String(problem));
    }
    cleanups.push(
      await onModelEvent((event) => {
        if (event.kind === "progress") {
          setFraction(event.fraction);
          return;
        }
        setDownloading(null);
        if (event.ok) {
          setDownloadDone(true);
          setFraction(1);
        } else if (event.cancelled) {
          setFraction(null);
        } else {
          setError(event.error ?? "The download failed.");
        }
      }),
    );
  });

  const startDownload = async (model: RecommendedModel) => {
    setError(null);
    setDownloading(model.file);
    setFraction(0);
    try {
      await api.downloadModel(model.repo, model.file);
    } catch (problem) {
      setDownloading(null);
      setError(String(problem));
    }
  };

  
  
  const finish = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.setProfile(userName(), assistantName(), replyMode());
      if (voiceOn()) await api.setVoiceEnabled(true).catch(() => undefined);
      await api.completeSetup();
      props.onDone();
    } catch (problem) {
      setError(String(problem));
    } finally {
      setBusy(false);
    }
  };

  const percent = () => (fraction() === null ? null : Math.round(fraction()! * 100));

  return (
    <div class="wizard-overlay">
      <div class="wizard-card">
        <h1>Welcome</h1>
        <p class="wizard-lead">
          A companion that lives on your desktop, thinks on your machine, and keeps your
          data there. Three quick things.
        </p>

        <ol class="wizard-progress">
          <For each={["Brain", "Names", "Voice"]}>
            {(label, index) => (
              <li classList={{ active: step() === index(), done: step() > index() }}>{label}</li>
            )}
          </For>
        </ol>

        {/* ── 1. the model ─────────────────────────────────────────────── */}
        <Show when={step() === 0}>
          <h2>Pick a brain</h2>
          <Show
            when={!downloadDone()}
            fallback={
              <p class="muted">
                A local model is installed. You can add more, or a cloud key, later.
              </p>
            }
          >
            <p class="muted small">
              This runs entirely offline on your GPU. It is a big download — it will keep
              going in the background while you finish setting up.
            </p>
            <div class="wizard-models">
              <For each={models()}>
                {(model) => (
                  <button
                    class="wizard-model"
                    disabled={downloading() !== null}
                    onClick={() => void startDownload(model)}
                  >
                    <strong>{model.name}</strong>
                    <span class="muted small">{model.note}</span>
                    <span class="muted small">
                      {model.params} · {model.quant} · ~{model.approx_gib} GiB
                    </span>
                  </button>
                )}
              </For>
            </div>
          </Show>

          <Show when={downloading()}>
            <div class="wizard-download">
              <div class="bar">
                <div class="bar-fill" style={{ width: `${percent() ?? 0}%` }} />
              </div>
              <span class="muted small">
                downloading… {percent() ?? 0}% — carry on, this continues in the background
              </span>
            </div>
          </Show>

          <div class="wizard-actions">
            <button
              class="wizard-primary"
              disabled={!downloading() && !downloadDone()}
              onClick={() => setStep(1)}
            >
              Next
            </button>
            <button class="ghost" onClick={() => setStep(1)}>
              I'll use a cloud key instead
            </button>
          </div>
        </Show>

        {/* ── 2. the names ─────────────────────────────────────────────── */}
        <Show when={step() === 1}>
          <h2>Introductions</h2>
          <p class="muted small">
            The agent is actually told these — it answers to its name and calls you by
            yours.
          </p>
          <div class="profile-grid">
            <label>
              What should it call you?
              <input
                type="text"
                value={userName()}
                placeholder="your name"
                onInput={(event) => setUserName(event.currentTarget.value)}
              />
            </label>
            <label>
              What do you call it?
              <input
                type="text"
                value={assistantName()}
                placeholder="e.g. Nova"
                onInput={(event) => setAssistantName(event.currentTarget.value)}
              />
            </label>
          </div>

          <h3>How should it answer?</h3>
          <div class="reply-modes">
            <For each={["read", "hear", "both"] as ReplyMode[]}>
              {(mode) => (
                <label class="reply-mode" classList={{ active: replyMode() === mode }}>
                  <input
                    type="radio"
                    name="setup-reply-mode"
                    checked={replyMode() === mode}
                    onChange={() => setReplyMode(mode)}
                  />
                  <strong>{mode === "read" ? "Read" : mode === "hear" ? "Hear" : "Both"}</strong>
                  <span class="muted small">
                    {mode === "read"
                      ? "a speech bubble"
                      : mode === "hear"
                        ? "spoken aloud"
                        : "bubble and voice"}
                  </span>
                </label>
              )}
            </For>
          </div>

          <div class="wizard-actions">
            <button class="ghost" onClick={() => setStep(0)}>
              Back
            </button>
            <button class="wizard-primary" onClick={() => setStep(2)}>
              Next
            </button>
          </div>
        </Show>

        {/* ── 3. the wake word ─────────────────────────────────────────── */}
        <Show when={step() === 2}>
          <h2>Teach it to hear its name</h2>
          <label class="wizard-voice">
            <input
              type="checkbox"
              checked={voiceOn()}
              onChange={(event) => setVoiceOn(event.currentTarget.checked)}
            />
            Let me talk to it (downloads speech models, ~210 MB)
          </label>

          <Show when={voiceOn()}>
            {/* The same component the dashboard's Voice panel uses — so a user who
                skips this, or has no microphone today, gets the identical thing
                later instead of a worse copy of it. */}
            <MicSetup assistantName={assistantName()} compact />
          </Show>

          <p class="muted small">
            You can skip all of this — the summon hotkey works as push-to-talk, and voice
            can be set up any time from the dashboard.
          </p>

          <div class="wizard-actions">
            <button class="ghost" onClick={() => setStep(1)}>
              Back
            </button>
            <button class="wizard-primary" disabled={busy()} onClick={() => void finish()}>
              {busy() ? "Finishing…" : "Done"}
            </button>
          </div>
        </Show>

        <Show when={error()}>
          <p class="error">{error()}</p>
        </Show>
      </div>
    </div>
  );
}

/**
 * The conversation. This is where typing lives now — the widget is the character,
 * and the character does not carry a text box.
 *
 * It shows the *same* thread the widget speaks from: the thread is owned by Rust
 * (`api.currentThread()`), so both windows join one conversation rather than each
 * creating their own.
 */
function ChatPane() {
  const chat = createChat();
  const [draft, setDraft] = createSignal("");
  const [copied, setCopied] = createSignal<number | null>(null);
  let transcript: HTMLDivElement | undefined;
  let fileInput: HTMLInputElement | undefined;

  const scrollToEnd = () =>
    queueMicrotask(() => transcript?.scrollTo({ top: transcript.scrollHeight }));

  createEffect(() => {
    chat.bubbles();
    scrollToEnd();
  });

  onMount(() => {
    const cleanups: (() => void)[] = [];
    onCleanup(() => cleanups.forEach((fn) => fn()));
    void (async () => {
      cleanups.push(await chat.start());
    })();
  });

  const submit = () => {
    const text = draft();
    if (!text.trim() || chat.busy()) return;
    setDraft("");
    void chat.send(text);
  };

  const copy = async (text: string, index: number) => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(index);
      setTimeout(() => setCopied((current) => (current === index ? null : current)), 1200);
    } catch {
      /* silent-ok: clipboard denied; the text is still selectable */
    }
  };

  /**
   * Upload is *attach as text*, not a file transfer: the agent reads what it is
   * given as part of the message. A binary file is refused rather than pasted in
   * as mojibake, and a large one is truncated with a visible marker rather than
   * silently cut — a model quietly fed half a document gives quietly wrong answers.
   */
  const MAX_ATTACH_BYTES = 128 * 1024;
  const attach = async (file: File) => {
    if (file.size > 8 * 1024 * 1024) {
      setDraft((current) => `${current}\n[${file.name} is too large to attach]`);
      return;
    }
    const text = await file.text();
    // A crude but reliable binary sniff: NUL bytes do not occur in text.
    if (text.includes(String.fromCharCode(0))) {
      setDraft((current) => `${current}\n[${file.name} looks binary; not attached]`);
      return;
    }
    const clipped =
      text.length > MAX_ATTACH_BYTES
        ? `${text.slice(0, MAX_ATTACH_BYTES)}\n… [truncated ${file.name}]`
        : text;
    setDraft(
      (current) =>
        `${current}${current ? "\n\n" : ""}--- ${file.name} ---\n${clipped}\n--- end ---\n\n`,
    );
  };

  return (
    <section class="chat-pane">
      <h2>
        Chat
        <button class="ghost right" title="New session" onClick={() => void chat.newSession()}>
          + New
        </button>
      </h2>

      <div class="chat-transcript" ref={transcript}>
        <Show when={chat.bubbles().length === 0}>
          <p class="muted">Nothing yet. Say something.</p>
        </Show>
        <For each={chat.bubbles()}>
          {(bubble, index) => (
            <div class={`chat-line ${bubble.role}`}>
              <div class="chat-text">{bubble.text}</div>
              <button
                class="ghost copy"
                title="Copy"
                onClick={() => void copy(bubble.text, index())}
              >
                {copied() === index() ? "copied" : "copy"}
              </button>
            </div>
          )}
        </For>
        <Show when={chat.statusLine()}>
          {(line) => <div class="chat-status">{line()}</div>}
        </Show>
      </div>

      <form
        class="chat-composer"
        onSubmit={(event) => {
          event.preventDefault();
          submit();
        }}
      >
        <textarea
          rows="3"
          placeholder={chat.busy() ? "Working…" : "Ask something"}
          value={draft()}
          disabled={!chat.ready()}
          onInput={(event) => setDraft(event.currentTarget.value)}
          onFocus={() => void api.setCharacterSignals({ listening: true }).catch(() => undefined)}
          onBlur={() => void api.setCharacterSignals({ listening: false }).catch(() => undefined)}
          onKeyDown={(event) => {
            // Enter sends; Shift+Enter is a newline. A multi-line composer that
            // sends on a bare Enter without this is unusable for pasting.
            if (event.key === "Enter" && !event.shiftKey) {
              event.preventDefault();
              submit();
            }
          }}
        />
        <div class="chat-actions">
          <input
            type="file"
            ref={fileInput}
            style="display: none"
            onChange={(event) => {
              const file = event.currentTarget.files?.[0];
              if (file) void attach(file);
              event.currentTarget.value = "";
            }}
          />
          <button
            type="button"
            class="ghost"
            title="Attach a text file"
            disabled={!chat.ready()}
            onClick={() => fileInput?.click()}
          >
            Attach
          </button>
          <Show
            when={chat.busy()}
            fallback={
              <button type="submit" disabled={!draft().trim() || !chat.ready()}>
                Send
              </button>
            }
          >
            {/* Always visible while a run is in flight, per the runaway-loop guardrails. */}
            <button type="button" class="stop" onClick={() => void chat.stop()}>
              Stop
            </button>
          </Show>
        </div>
      </form>
    </section>
  );
}

/**
 * Who the user is, what the assistant is called, and how it answers.
 *
 * The names are not decoration: they are written into the agent's system prompt,
 * so the model actually knows them. There is no account here — this is a
 * single-user desktop app, and this is the profile, not a login.
 */
function ProfilePanel() {
  const [userName, setUserName] = createSignal("");
  const [assistantName, setAssistantName] = createSignal("");
  const [replyMode, setReplyMode] = createSignal<ReplyMode>("read");
  const [hotkey, setHotkey] = createSignal<string | null>(null);
  const [saved, setSaved] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  onMount(async () => {
    try {
      const profile = await api.profile();
      setUserName(profile.user_name);
      setAssistantName(profile.assistant_name);
      setReplyMode(profile.reply_mode);
      setHotkey(await api.summonHotkey());
    } catch (problem) {
      setError(String(problem));
    }
  });

  const save = async () => {
    setError(null);
    try {
      await api.setProfile(userName(), assistantName(), replyMode());
      setSaved(true);
      setTimeout(() => setSaved(false), 1500);
    } catch (problem) {
      setError(String(problem));
    }
  };

  const rebind = async (binding: string) => {
    setError(null);
    try {
      await api.setSummonHotkey(binding);
      setHotkey(binding);
    } catch (problem) {
      // The old binding is still live — say so, rather than leaving the user
      // believing they have a hotkey they do not.
      setError(String(problem));
      setHotkey(await api.summonHotkey().catch(() => null));
    }
  };

  const MODES: ReplyMode[] = ["read", "hear", "both"];
  const describe = (mode: ReplyMode) =>
    mode === "read"
      ? "speech bubble only"
      : mode === "hear"
        ? "spoken aloud, no bubble"
        : "bubble and spoken";

  return (
    <section>
      <h2>You &amp; your assistant</h2>
      <div class="profile-grid">
        <label>
          Your name
          <input
            type="text"
            value={userName()}
            placeholder="what should it call you?"
            onInput={(event) => setUserName(event.currentTarget.value)}
          />
        </label>
        <label>
          Assistant&apos;s name
          <input
            type="text"
            value={assistantName()}
            placeholder="what do you call it?"
            onInput={(event) => setAssistantName(event.currentTarget.value)}
          />
        </label>
      </div>
      <p class="muted small">
        These go into the agent&apos;s system prompt — it genuinely knows them, and answers
        to its name.
      </p>

      <h3>How it answers</h3>
      <div class="reply-modes">
        <For each={MODES}>
          {(mode) => (
            <label class="reply-mode" classList={{ active: replyMode() === mode }}>
              <input
                type="radio"
                name="reply-mode"
                checked={replyMode() === mode}
                onChange={() => setReplyMode(mode)}
              />
              <strong>{mode === "read" ? "Read" : mode === "hear" ? "Hear" : "Both"}</strong>
              <span class="muted small">{describe(mode)}</span>
            </label>
          )}
        </For>
      </div>
      <p class="muted small">Hearing needs voice enabled, which downloads speech models.</p>

      <h3>Summon hotkey</h3>
      <p class="muted small">
        Wakes the character, and while voice is on doubles as push-to-talk.
        {hotkey() ? "" : " Nothing is bound — every candidate was taken."}
      </p>
      <div class="hotkey-row">
        <For each={["Ctrl+Alt+Space", "Ctrl+Shift+Space", "Ctrl+Alt+A", "Ctrl+Shift+A"]}>
          {(binding) => (
            <button
              class="ghost"
              classList={{ active: hotkey() === binding }}
              onClick={() => void rebind(binding)}
            >
              {binding}
            </button>
          )}
        </For>
      </div>

      <div class="row">
        <button onClick={() => void save()}>Save</button>
        <Show when={saved()}>
          <span class="muted small">saved</span>
        </Show>
      </div>
      <Show when={error()}>
        <p class="error">{error()}</p>
      </Show>
    </section>
  );
}

/**
 * Microphone + wake word. Used by the setup wizard *and* by a permanent dashboard
 * panel, because "set up voice" is not a one-time event: the user may skip it at
 * first run, have no microphone that day, or want to retrain the wake word after
 * changing the assistant's name.
 *
 * The device picker is not a nicety. The OS default input is frequently a paired
 * Bluetooth headset whose HFP endpoint opens cleanly and then delivers near-silence
 * — nothing errors, the user simply is not heard. Measured on this machine: the
 * default peaked at 0.003 while the real microphone sat unused. So the device is a
 * choice, and the level meter is how the user can *see* which one hears them.
 */
/**
 * Below this peak there is no voice in the recording at all — a muted mic or a dead
 * endpoint. Measured, not guessed: a person speaking normally into a Bluetooth
 * headset peaks around 0.03, so a bar at 0.02 would call a slightly quiet speaker
 * "silent" and send them hunting for a hardware fault they do not have.
 * Mirrors `ic_voice::train::SILENCE_PEAK`.
 */
const SILENCE_PEAK = 0.005;

function MicSetup(props: {
  assistantName: string;
  compact?: boolean;
  onTrained?: () => void;
}) {
  const [devices, setDevices] = createSignal<string[]>([]);
  const [device, setDevice] = createSignal<string | null>(null);
  const [level, setLevel] = createSignal<number | null>(null);
  const [testing, setTesting] = createSignal(false);
  const [takes, setTakes] = createSignal(0);
  const [needed, setNeeded] = createSignal(3);
  const [recording, setRecording] = createSignal(false);
  const [quiet, setQuiet] = createSignal(false);
  const [training, setTraining] = createSignal(false);
  const [trained, setTrained] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  onMount(async () => {
    try {
      setDevices(await api.inputDevices());
      const profile = await api.voiceSettings();
      setDevice(profile.input_device);
    } catch (problem) {
      setError(String(problem));
    }
  });

  const choose = async (name: string) => {
    setError(null);
    setLevel(null);
    setDevice(name);
    try {
      await api.setInputDevice(name);
    } catch (problem) {
      setError(String(problem));
    }
  };

  const test = async () => {
    setError(null);
    setTesting(true);
    try {
      setLevel(await api.testMicrophone());
    } catch (problem) {
      setError(String(problem));
    } finally {
      setTesting(false);
    }
  };

  const record = async () => {
    setError(null);
    setRecording(true);
    setQuiet(false);
    try {
      const sample = await api.recordWakeSample();
      setTakes(sample.recorded);
      setNeeded(sample.needed);
      // Say so on take one, not after three wasted takes and a failed training run.
      setQuiet(sample.peak < SILENCE_PEAK);
    } catch (problem) {
      setError(String(problem));
    } finally {
      setRecording(false);
    }
  };

  const train = async () => {
    setError(null);
    setTraining(true);
    try {
      await api.trainWakeWord(props.assistantName);
      setTrained(true);
      props.onTrained?.();
    } catch (problem) {
      setError(String(problem));
    } finally {
      setTraining(false);
    }
  };

  const restart = () => {
    setTakes(0);
    setTrained(false);
    setQuiet(false);
    void api.resetWakeSamples().catch(() => undefined);
  };

  const heard = () => level() !== null && level()! >= SILENCE_PEAK;

  return (
    <div class="mic-setup">
      <h3>Microphone</h3>
      <p class="muted small">
        Windows often defaults to a Bluetooth headset that opens fine and hears nothing.
        Pick the one that moves the bar.
      </p>
      <div class="mic-devices">
        <For each={devices()}>
          {(name, index) => (
            <button
              class="ghost mic-device"
              classList={{ active: device() === name || (device() === null && index() === 0) }}
              onClick={() => void choose(name)}
            >
              {name}
              <Show when={index() === 0}>
                <span class="muted small"> (system default)</span>
              </Show>
            </button>
          )}
        </For>
      </div>

      <div class="mic-test">
        <button class="ghost" disabled={testing()} onClick={() => void test()}>
          {testing() ? "Listening…" : "Test — say something"}
        </button>
        <Show when={level() !== null}>
          <div class="level">
            <div
              class="level-fill"
              classList={{ good: heard() }}
              style={{ width: `${Math.min(100, Math.round(level()! * 400))}%` }}
            />
          </div>
          <span class="muted small" classList={{ error: !heard() }}>
            {heard()
              ? "heard you"
              : "nothing came through — try another microphone, or unmute it"}
          </span>
        </Show>
      </div>

      <h3>Wake word</h3>
      <Show
        when={props.assistantName.trim()}
        fallback={
          <p class="muted small">
            Name the assistant first — its name <em>is</em> the wake word.
          </p>
        }
      >
        <p class="muted small">
          Say <strong>“{props.assistantName}”</strong> {needed()} times. The recordings are
          turned into a small model on this machine and never leave it.
        </p>

        <div class="wake-takes">
          <For each={Array.from({ length: needed() })}>
            {(_, index) => <span class="wake-dot" classList={{ filled: takes() > index() }} />}
          </For>
        </div>

        <Show when={quiet()}>
          <p class="error small">That take was silent — check the microphone above.</p>
        </Show>

        <div class="row">
          <Show
            when={takes() >= needed()}
            fallback={
              <button disabled={recording()} onClick={() => void record()}>
                {recording() ? "Listening…" : `Record “${props.assistantName}”`}
              </button>
            }
          >
            <Show when={!trained()} fallback={<span class="muted small">Wake word ready.</span>}>
              <button disabled={training()} onClick={() => void train()}>
                {training() ? "Learning…" : "Teach it"}
              </button>
            </Show>
          </Show>
          <Show when={takes() > 0}>
            <button class="ghost" onClick={restart}>
              Start over
            </button>
          </Show>
        </div>

        <Show when={trained()}>
          <p class="muted small">
            It takes effect the next time voice starts — restart the app, or toggle voice
            off and on.
          </p>
        </Show>
      </Show>

      <Show when={!props.compact}>
        <p class="muted small">
          Push-to-talk works either way: hold the summon hotkey and speak.
        </p>
      </Show>

      <Show when={error()}>
        <p class="error">{error()}</p>
      </Show>
    </div>
  );
}

/**
 * Voice, permanently available — not only during first-run setup.
 *
 * The wizard is a moment; a microphone is a lifetime. A user who skipped voice, had
 * no mic that day, or renamed the assistant needs to come back to this, and before
 * this panel existed the only way to see the wake-word step again was to wipe
 * `setup_complete` by hand.
 */
function VoicePanel() {
  const [enabled, setEnabled] = createSignal(false);
  const [muted, setMuted] = createSignal(false);
  const [running, setRunning] = createSignal(false);
  const [assistantName, setAssistantName] = createSignal("");
  const [error, setError] = createSignal<string | null>(null);
  const [state, setState] = createSignal<VoiceState>("idle");
  const [heard, setHeard] = createSignal<string | null>(null);
  const [wakeWord, setWakeWord] = createSignal(false);

  const refresh = async () => {
    try {
      const status = await api.voiceStatus();
      setEnabled(status.enabled);
      setMuted(status.muted);
      setRunning(status.running);
      setAssistantName((await api.profile()).assistant_name);
    } catch (problem) {
      setError(String(problem));
    }
  };

  onMount(() => {
    void refresh();
    const cleanups: (() => void)[] = [];
    onCleanup(() => cleanups.forEach((fn) => fn()));
    void (async () => {
      cleanups.push(await onVoiceState(setState));
      cleanups.push(await onVoiceTranscript((text) => setHeard(text.trim())));
      setWakeWord(await api.hasWakeWord().catch(() => false));
    })();
  });

  /**
   * Why voice is not listening, in the user's terms. Every one of these was
   * previously invisible: the app simply did nothing and the user had no way to tell
   * a muted mic from a deaf device from a wake word that was never trained.
   */
  const blocker = () => {
    if (!enabled()) return "Voice is off.";
    if (!running()) return "Starting — the speech models are still downloading.";
    if (muted()) return "The microphone is muted. Nothing is being heard.";
    if (!wakeWord())
      return `No wake word yet — say nothing, just hold ${"the summon hotkey"} and speak (push-to-talk).`;
    return null;
  };

  const toggleEnabled = async (on: boolean) => {
    setError(null);
    setEnabled(on);
    try {
      await api.setVoiceEnabled(on);
      await refresh();
    } catch (problem) {
      setError(String(problem));
    }
  };

  return (
    <section>
      <h2>Voice</h2>
      <label class="wizard-voice">
        <input
          type="checkbox"
          checked={enabled()}
          onChange={(event) => void toggleEnabled(event.currentTarget.checked)}
        />
        Let me talk to it
        <Show when={enabled() && !running()}>
          <span class="muted small"> — starting (first time downloads ~210 MB)</span>
        </Show>
      </label>

      <Show when={enabled()}>
        <label class="wizard-voice">
          <input
            type="checkbox"
            checked={muted()}
            onChange={async (event) => {
              const next = event.currentTarget.checked;
              setMuted(next);
              await api.setVoiceMuted(next).catch((problem) => setError(String(problem)));
            }}
          />
          Mute the microphone
        </label>
      </Show>

      <Show when={blocker()}>
        {(why) => <p class="reason-inline">{why()}</p>}
      </Show>

      <Show when={enabled() && running()}>
        <div class="row">
          <button
            disabled={muted() || state() !== "idle"}
            onClick={() =>
              void api.startListening().catch((problem) => setError(String(problem)))
            }
          >
            {state() === "listening" ? "Listening…" : "Talk to it"}
          </button>
          <span class="muted small">or press the summon hotkey</span>
        </div>
        <p class="muted small">
          Status: <strong>{state()}</strong>
          <Show when={heard() !== null}>
            {" — last heard: "}
            <Show when={heard()} fallback={<em>nothing</em>}>
              <strong>“{heard()}”</strong>
            </Show>
          </Show>
        </p>
      </Show>

      <MicSetup assistantName={assistantName()} onTrained={() => setWakeWord(true)} />

      <Show when={error()}>
        <p class="error">{error()}</p>
      </Show>
    </section>
  );
}

/**
 * Ambient mode: whether the character may speak first (Phase 7a).
 *
 * The toggle does two things, and the panel says both out loud. It lets the widget
 * surface things nobody asked for, and it switches on the gateway's trigger poller
 * — without which a scheduled automation is listed but never actually fires. That
 * is also why turning it on restarts the gateway: the runtime reads the poller
 * switch once, at boot.
 */
function AmbientPanel() {
  const [status, setStatus] = createSignal<AmbientStatus | null>(null);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const refresh = async () => {
    try {
      setStatus(await api.ambientStatus());
    } catch (problem) {
      setError(String(problem));
    }
  };
  onMount(() => void refresh());

  const toggle = async (on: boolean) => {
    setBusy(true);
    setError(null);
    try {
      await api.setAmbientEnabled(on);
      // The gateway restarts, and both windows reload with it — so there may be
      // nothing left to refresh. Harmless if there is.
      await refresh();
    } catch (problem) {
      setError(String(problem));
    } finally {
      setBusy(false);
    }
  };

  const saveGuardrails = async (max: number, start: number | null, end: number | null) => {
    setError(null);
    try {
      await api.setAmbientGuardrails(max, start, end);
      await refresh();
    } catch (problem) {
      setError(String(problem));
    }
  };

  const quietLabel = () => {
    const current = status();
    if (!current || current.quiet_start === null || current.quiet_end === null) return "none";
    return `${String(current.quiet_start).padStart(2, "0")}:00 – ${String(current.quiet_end).padStart(2, "0")}:00`;
  };

  return (
    <section>
      <h2>Ambient</h2>
      <label class="wizard-voice">
        <input
          type="checkbox"
          disabled={busy()}
          checked={status()?.enabled ?? false}
          onChange={(event) => void toggle(event.currentTarget.checked)}
        />
        Let it speak first
        <Show when={busy()}>
          <span class="muted small"> — restarting the agent…</span>
        </Show>
      </label>
      <p class="muted small">
        Off by default. When it is off, scheduled automations never run — the agent
        only ever acts on something you asked for.
      </p>

      <Show when={status()?.enabled}>
        <div class="row">
          <label>
            At most{" "}
            <input
              type="number"
              min="1"
              max="20"
              class="tiny"
              value={status()?.max_per_hour ?? 2}
              onChange={(event) =>
                void saveGuardrails(
                  Number(event.currentTarget.value),
                  status()?.quiet_start ?? null,
                  status()?.quiet_end ?? null,
                )
              }
            />{" "}
            interruptions an hour
          </label>
        </div>
        <p class="muted small">
          Quiet hours: <strong>{quietLabel()}</strong>
          {" · "}
          <button
            class="ghost"
            onClick={() =>
              void saveGuardrails(
                status()?.max_per_hour ?? 2,
                status()?.quiet_start === null ? 22 : null,
                status()?.quiet_start === null ? 8 : null,
              )
            }
          >
            {status()?.quiet_start === null ? "Quiet 22:00 – 08:00" : "Never go quiet"}
          </button>
        </p>
      </Show>

      <Show when={error()}>
        <p class="error">{error()}</p>
      </Show>
    </section>
  );
}

function Dashboard() {
  const [gateway, setGateway] = createSignal<GatewayState>({ state: "starting" });
  const [log, setLog] = createSignal("");
  const [showWizard, setShowWizard] = createSignal(false);

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

    // Show the first-run wizard until the user completes it.
    setShowWizard(await api.needsSetup().catch(() => false));
  });

  const refreshLog = async () => setLog(await api.gatewayLog());

  return (
    <div class="dashboard">
      <Show when={showWizard()}>
        <SetupWizard onDone={() => setShowWizard(false)} />
      </Show>
      <h1>IronClaw Desktop</h1>

      <ChatPane />
      <ProfilePanel />
      <VoicePanel />
      <AmbientPanel />

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
          Scheduled entries only — <code>serve</code> exposes no run history. They
          only fire while <strong>Ambient</strong> is on: the gateway's trigger
          poller is off otherwise.
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

      <ModelsPanel />

      <CharacterPanel />

      <ProviderPanel />

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
