/**
 * The character rendering layer.
 *
 * Phase 3 puts an animated character on the desktop. *What* it does per state is
 * decided on the backend (`ic_widget::character`) and arrives as
 * `character://state`; *how* it is drawn is a `CharacterRenderer`. Three
 * backends live behind that one interface:
 *
 *   - `Live2DRenderer` — pixi-live2d-display over the Cubism Core. Blinks and
 *     breathes on its own (the Cubism framework drives both), follows the
 *     cursor (`focus`), plays the mapped expression/motion per state, and moves
 *     its mouth from a test-tone stub while speaking (real TTS amplitude lands
 *     in Phase 5).
 *   - `SpriteRenderer` — flat PNG poses with cheap-puppet CSS transforms. The
 *     fallback for user-supplied art; nothing ships with it (dev-only assets
 *     must not reach public releases — CLAUDE.md Phase 3).
 *   - `PlaceholderRenderer` — a state-labelled emoji face. No assets, no SDK;
 *     the pipeline stays verifiable with nothing installed.
 *
 * A character is an asset folder plus a `character.json` ([`CharacterConfig`])
 * mapping each state to renderer-specific animation. Swapping characters or
 * backends is data, not code.
 */

import * as PIXI from "pixi.js";
import { install } from "@pixi/unsafe-eval";
import { Live2DModel, MotionPriority } from "pixi-live2d-display/cubism4";

import { api, type CharacterState } from "./api";

// PixiJS builds its shader/batch code with `new Function`, which the widget's
// CSP forbids (no `unsafe-eval`). This module — despite its name — patches Pixi
// to a non-eval path so it runs *under* that CSP. It must run before any
// renderer is created.
install(PIXI);

// Texture upload workaround for WebView2 + ANGLE/D3D11.
//
// In this environment, uploading an HTMLImageElement *or* an ImageBitmap to
// WebGL produced a blank (fully transparent) texture in earlier probes, while a
// 2D canvas uploaded correctly. (Those probes ran while a dead render ticker
// masked everything, so this may be removable — tracked in
// docs/desktop/character-pipeline.md.)
//
// pixi-live2d-display loads every model texture through `Texture.fromURL`, so
// route that one method through <img> → canvas → texture. Nothing else in the
// app uses Pixi, so the override is contained.
(
  PIXI.Texture as unknown as {
    fromURL: (url: string | string[]) => Promise<PIXI.Texture>;
  }
).fromURL = (url) => {
  const src = Array.isArray(url) ? url[0] : url;
  return new Promise<PIXI.Texture>((resolve, reject) => {
    if (!src) {
      reject(new Error("empty texture url"));
      return;
    }
    const image = new Image();
    image.onload = () => {
      const canvas = document.createElement("canvas");
      canvas.width = image.naturalWidth;
      canvas.height = image.naturalHeight;
      const ctx = canvas.getContext("2d");
      if (!ctx) {
        reject(new Error("no 2d context for the texture canvas"));
        return;
      }
      ctx.drawImage(image, 0, 0);
      resolve(PIXI.Texture.from(canvas));
    };
    image.onerror = () => reject(new Error(`texture load failed: ${src}`));
    image.src = src;
  });
};

// pixi-live2d-display drives motions and physics off the shared ticker.
Live2DModel.registerTicker(PIXI.Ticker);
// Phase 3 performance rule: ~30 fps is plenty for an idle companion, and the
// saved GPU headroom belongs to llama.cpp. The app render ticker gets the same
// cap in `Live2DRenderer.mount`.
PIXI.Ticker.shared.maxFPS = 30;

// Cubism Core compatibility bridge.
//
// The core shipped in `ui/public/live2d/` is the Cubism 5 web core (SDK 5-r.5,
// core version 6). It moved render orders from `drawables.renderOrders` to
// `model.getRenderOrders()` — one combined ordering covering drawables plus the
// new "offscreens". pixi-live2d-display 0.4.0 bundles the Cubism *4* framework,
// whose `doDrawModel` still reads the old field: `renderOrder[i]` on undefined
// threw on the first rendered frame, and because a throw in a Pixi ticker
// callback ends the rAF chain (`Ticker._tick` only re-requests after `update()`
// returns), the app ticker died and the canvas stayed blank forever — while the
// model's own updates kept ticking on the shared ticker.
//
// Everything else the Cubism 4 framework touches still exists on this core
// (verified against a live model: parameters, parts, canvasinfo, dynamic-flag
// utils). For a model with no offscreens the combined ordering is exactly the
// drawable ordering, so exposing the new array under the old name is faithful.
// Models that DO use offscreens (Cubism 5 blend features — Ren) get a logged
// warning; they need the Cubism 5 framework to render correctly anyway.
interface CoreModelLike {
  drawables?: { renderOrders?: unknown };
  offscreens?: { count?: number };
  getRenderOrders?: () => Int32Array;
}
let coreBridged = false;
function bridgeCoreRenderOrders(): void {
  if (coreBridged) return;
  const core = (
    window as { Live2DCubismCore?: { Model?: { fromMoc?: (moc: unknown) => unknown } } }
  ).Live2DCubismCore;
  const modelClass = core?.Model;
  if (!modelClass?.fromMoc) return;
  const fromMoc = modelClass.fromMoc.bind(modelClass);
  modelClass.fromMoc = (moc: unknown) => {
    const model = fromMoc(moc) as CoreModelLike | null;
    if (
      model?.drawables &&
      model.drawables.renderOrders === undefined &&
      typeof model.getRenderOrders === "function"
    ) {
      if ((model.offscreens?.count ?? 0) > 0) {
        void api.logUiError(
          "character: model uses offscreens; the render-order bridge may misorder drawables",
        );
      }
      Object.defineProperty(model.drawables, "renderOrders", {
        get: () => model.getRenderOrders!(),
      });
    }
    return model;
  };
  coreBridged = true;
}

/** How one state maps onto a renderer's animation. Fields are per-backend. */
export interface StateMapping {
  /** Live2D: an expression id and/or a motion group from the `.model3.json`. */
  live2d?: { expression?: string; motion?: string };
  /** Sprite: the pose image within the character's `sprite/` folder. */
  sprite?: string;
}

/** A character's `character.json`. */
export interface CharacterConfig {
  /** Display name. */
  name: string;
  /** Which backend draws it. */
  renderer: "live2d" | "sprite" | "placeholder";
  /** Live2D: URL of the `.model3.json`. Required when `renderer` is `live2d`. */
  model?: string;
  /** Fraction of the container the character fills (`1` = fit exactly). */
  scale: number;
  /** Anchor within its box, `0..1` (feet-centre is `{ x: 0.5, y: 1 }`). */
  anchor: { x: number; y: number };
  /** State → animation. Every state is present so a renderer never guesses. */
  states: Record<CharacterState, StateMapping>;
  /** The folder the config was loaded from; set by `loadCharacterConfig`. */
  baseUrl?: string;
}

/** Fetch and parse a `character.json`. */
export async function loadCharacterConfig(url: string): Promise<CharacterConfig> {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`character config ${url}: HTTP ${response.status}`);
  }
  const config = (await response.json()) as CharacterConfig;
  config.baseUrl = url.slice(0, url.lastIndexOf("/"));
  return config;
}

/** Where a click landed on the character. */
export type CharacterHit = "head" | "body";

/**
 * The solid region a renderer contributes to the window's click-through mask.
 * Coordinates are window-local logical pixels.
 */
export type HitProfile =
  | { kind: "rect"; left: number; top: number; width: number; height: number }
  | {
      kind: "cells";
      /** Cell edge, logical px (matches the caller's request). */
      cell: number;
      originX: number;
      originY: number;
      cols: number;
      rows: number;
      /** Row-major, one byte per cell, nonzero = solid. */
      solid: Uint8Array;
    };

/** The contract every backend implements. */
export interface CharacterRenderer {
  /** Attach to `container`. May load assets, so it can be async. */
  mount(container: HTMLElement): Promise<void> | void;
  /** Show `state`. Called on every `character://state`; must be cheap. */
  setState(state: CharacterState): void;
  /** Detach and free resources. */
  destroy(): void;
  /** Look toward a window-local point (cursor following). Optional. */
  focus?(x: number, y: number): void;
  /** Pause/resume animation (fullscreen app foreground, Phase 3 perf). */
  setActive?(active: boolean): void;
  /** What part of the character sits at a window-local point, if any. */
  hitAt?(x: number, y: number): CharacterHit | null;
  /** The character's solid region for the click-through mask. */
  hitProfile?(cell: number): HitProfile | null;
}

/** The built-in dev character, used until a real asset folder lands. */
export const PLACEHOLDER_CONFIG: CharacterConfig = {
  name: "Placeholder",
  renderer: "placeholder",
  scale: 1,
  anchor: { x: 0.5, y: 1 },
  states: {
    idle: {},
    listening: {},
    thinking: {},
    speaking: {},
    concerned: {},
    error: {},
  },
};

/** The face shown for each state by the placeholder. */
const FACES: Record<CharacterState, string> = {
  idle: "🙂",
  listening: "👂",
  thinking: "🤔",
  speaking: "🗣️",
  concerned: "😟",
  error: "😵",
};

/** A one-rect profile for a DOM element, in window-local logical pixels. */
function elementProfile(element: HTMLElement | undefined): HitProfile | null {
  if (!element) return null;
  const rect = element.getBoundingClientRect();
  if (rect.width <= 0 || rect.height <= 0) return null;
  return { kind: "rect", left: rect.left, top: rect.top, width: rect.width, height: rect.height };
}

/**
 * A no-assets renderer: a large emoji whose face follows the state.
 *
 * `data-state` is set on the element too, so CSS can add per-state motion (a
 * pulse while thinking, a shake on error) without this class knowing about it.
 */
export class PlaceholderRenderer implements CharacterRenderer {
  private face?: HTMLElement;

  mount(container: HTMLElement): void {
    const face = document.createElement("div");
    face.className = "character-face";
    face.dataset.state = "idle";
    face.textContent = FACES.idle;
    container.appendChild(face);
    this.face = face;
  }

  setState(state: CharacterState): void {
    if (!this.face) return;
    this.face.dataset.state = state;
    this.face.textContent = FACES[state];
  }

  destroy(): void {
    this.face?.remove();
    this.face = undefined;
  }

  hitAt(x: number, y: number): CharacterHit | null {
    const rect = this.face?.getBoundingClientRect();
    if (!rect || x < rect.left || x >= rect.right || y < rect.top || y >= rect.bottom) {
      return null;
    }
    // An emoji has no anatomy; the whole face summons.
    return "head";
  }

  hitProfile(): HitProfile | null {
    return elementProfile(this.face);
  }
}

/**
 * Flat PNG poses with cheap-puppet transforms (idle bob, tilt, squash — all in
 * CSS keyed off `data-state`). Pose files come from the config's `sprite`
 * mappings, resolved against `<baseUrl>/sprite/`.
 */
export class SpriteRenderer implements CharacterRenderer {
  private readonly config: CharacterConfig;
  private image?: HTMLImageElement;

  constructor(config: CharacterConfig) {
    this.config = config;
  }

  private poseUrl(state: CharacterState): string | null {
    const file = this.config.states[state]?.sprite ?? this.config.states.idle?.sprite;
    if (!file || !this.config.baseUrl) return null;
    return `${this.config.baseUrl}/sprite/${file}`;
  }

  async mount(container: HTMLElement): Promise<void> {
    const url = this.poseUrl("idle");
    if (!url) throw new Error(`character "${this.config.name}" has no idle sprite`);
    const image = document.createElement("img");
    image.className = "character-sprite";
    image.dataset.state = "idle";
    image.draggable = false;
    image.src = url;
    // A missing pose must fall back to the placeholder, not show a broken-image
    // glyph standing on the desktop — decode() rejects on load failure.
    await image.decode();
    container.appendChild(image);
    this.image = image;
  }

  setState(state: CharacterState): void {
    if (!this.image) return;
    this.image.dataset.state = state;
    const url = this.poseUrl(state);
    // A state without art keeps the previous pose; the CSS transform still
    // reacts through `data-state`.
    if (url && !this.image.src.endsWith(url)) this.image.src = url;
  }

  destroy(): void {
    this.image?.remove();
    this.image = undefined;
  }

  hitAt(x: number, y: number): CharacterHit | null {
    const rect = this.image?.getBoundingClientRect();
    if (!rect || x < rect.left || x >= rect.right || y < rect.top || y >= rect.bottom) {
      return null;
    }
    return y < rect.top + rect.height * 0.35 ? "head" : "body";
  }

  hitProfile(): HitProfile | null {
    return elementProfile(this.image);
  }
}

/** What `Live2DRenderer` reaches into pixi-live2d-display for. */
interface CubismInternals {
  coreModel?: {
    setParameterValueById?: (id: string, value: number, weight?: number) => void;
  };
  motionManager?: { update?: (...args: unknown[]) => boolean };
  /** The `.model3.json` `LipSync` group ids, e.g. `["ParamMouthOpenY"]`. */
  lipSyncIds?: string[];
}

/**
 * A Live2D character, drawn with pixi-live2d-display over the Cubism Core.
 *
 * The Core is a global (`window.Live2DCubismCore`) loaded by `index.html` before
 * the app; its WebAssembly needs `wasm-unsafe-eval` in the CSP. State changes
 * set the mapped expression and play the mapped motion group; blink, breath,
 * and physics run inside the Cubism framework without help.
 */
export class Live2DRenderer implements CharacterRenderer {
  private readonly config: CharacterConfig;
  private app?: PIXI.Application;
  private model?: Live2DModel;
  /** `performance.now()` when speaking began, or `null` when not speaking. */
  private speakingSince: number | null = null;

  constructor(config: CharacterConfig) {
    this.config = config;
  }

  async mount(container: HTMLElement): Promise<void> {
    const modelUrl = this.config.model;
    if (!modelUrl) throw new Error(`character "${this.config.name}" has no model url`);
    bridgeCoreRenderOrders();

    // Size the renderer explicitly. `onMount` can fire before the container has
    // been laid out, and a `resizeTo` that measures 0×0 leaves the canvas 0×0 —
    // the model loads but nothing is ever drawn. Fall back to sensible
    // dimensions, then keep tracking the container for later resizes.
    const width = container.clientWidth || 360;
    const height = container.clientHeight || 320;
    const app = new PIXI.Application({
      width,
      height,
      resizeTo: container,
      backgroundAlpha: 0,
      antialias: true,
      autoDensity: true,
      resolution: window.devicePixelRatio || 1,
    });
    app.ticker.maxFPS = 30;
    container.appendChild(app.view as unknown as HTMLCanvasElement);

    // A throw inside a Pixi ticker callback ends the rAF chain — `Ticker._tick`
    // only re-requests the next frame after `update()` returns — so one bad
    // render frame would stop rendering forever with a single console error.
    // Own the render pass instead: contain the error, report it once, keep the
    // loop alive.
    app.ticker.remove(app.render, app);
    let renderErrors = 0;
    app.ticker.add(() => {
      try {
        app.render();
      } catch (error) {
        renderErrors += 1;
        if (renderErrors === 1) {
          void api.logUiError(`character render loop: ${String(error)}`);
        }
      }
    });

    // `autoInteract` would register Pixi's InteractionManager for hit/focus.
    // The window is click-through over empty pixels, so the webview cannot see
    // the cursor reliably — Rust polls it and the widget calls `focus`/`hitAt`.
    const model = await Live2DModel.from(modelUrl, { autoInteract: false });
    app.stage.addChild(model);
    this.app = app;
    this.model = model;
    this.fit();
    this.patchLipSync();
  }

  /** Scale the model to `config.scale` of the renderer and place it by anchor. */
  private fit(): void {
    if (!this.model || !this.app) return;
    const w = this.app.screen.width;
    const h = this.app.screen.height;
    const mw = this.model.width;
    const mh = this.model.height;
    // Guard the divide: an unmeasured model would scale to Infinity/NaN and
    // vanish. Leave the default scale until real bounds exist.
    if (mw > 0 && mh > 0) {
      this.model.scale.set(Math.min(w / mw, h / mh) * this.config.scale);
    }
    this.model.anchor.set(this.config.anchor.x, this.config.anchor.y);
    this.model.position.set(w * this.config.anchor.x, h * this.config.anchor.y);
  }

  /**
   * Drive the mouth while speaking. The Cubism motion pipeline writes every
   * parameter each frame, so the write has to happen *inside* the update —
   * after the motion, before physics/pose — which is exactly where
   * `motionManager.update` sits. Patching it is the standard pixi-live2d-display
   * lip-sync seam.
   */
  private patchLipSync(): void {
    const internal = (this.model as unknown as { internalModel?: CubismInternals })
      .internalModel;
    const manager = internal?.motionManager;
    const core = internal?.coreModel;
    if (!manager?.update || !core?.setParameterValueById) return;
    const paramId = internal?.lipSyncIds?.[0] ?? "ParamMouthOpenY";
    const original = manager.update.bind(manager);
    manager.update = (...args: unknown[]) => {
      const updated = original(...args);
      if (this.speakingSince === null) return updated;
      // Test-tone stub: a syllable-ish envelope until Piper TTS supplies real
      // playback amplitude (Phase 5 wires that into this same parameter).
      const t = (performance.now() - this.speakingSince) / 1000;
      const envelope = 0.55 + 0.45 * Math.sin(t * 1.9);
      const syllables = Math.abs(Math.sin(t * 7.3));
      core.setParameterValueById!(paramId, Math.min(1, syllables * envelope), 0.9);
      return true;
    };
  }

  setState(state: CharacterState): void {
    const mapping = this.config.states[state]?.live2d;
    this.speakingSince = state === "speaking" ? performance.now() : null;
    if (!mapping || !this.model) return;
    if (mapping.expression) this.model.expression(mapping.expression);
    if (mapping.motion) {
      // FORCE so a state change interrupts whatever is playing (Phase 3:
      // transitions are interruptible). The idle group is the one exception —
      // the motion manager already loops it at idle priority, and forcing it
      // would restart the loop on every return to rest.
      if (state === "idle") return;
      void this.model.motion(mapping.motion, undefined, MotionPriority.FORCE);
    }
  }

  focus(x: number, y: number): void {
    if (!this.model || !this.app) return;
    const view = this.app.view as HTMLCanvasElement;
    const rect = view.getBoundingClientRect();
    // The model looks toward the cursor even when it is outside the canvas —
    // clamp so the head does not wrench past its parameter range.
    const cx = Math.min(Math.max(x - rect.left, 0), rect.width);
    const cy = Math.min(Math.max(y - rect.top, 0), rect.height);
    this.model.focus(cx, cy);
  }

  setActive(active: boolean): void {
    if (!this.app) return;
    // Both tickers: the app ticker renders, the shared ticker updates the model.
    const tickers = [this.app.ticker, PIXI.Ticker.shared];
    for (const ticker of tickers) {
      if (active && !ticker.started) ticker.start();
      if (!active && ticker.started) ticker.stop();
    }
  }

  hitAt(x: number, y: number): CharacterHit | null {
    if (!this.model || !this.app) return null;
    const view = this.app.view as HTMLCanvasElement;
    const rect = view.getBoundingClientRect();
    const cx = x - rect.left;
    const cy = y - rect.top;
    // The model's own hit areas first (Ren has Head + Body; Hiyori only Body).
    const areas = this.model.hitTest(cx, cy).map((name) => name.toLowerCase());
    if (areas.includes("head")) return "head";
    if (areas.length > 0) return "body";
    // No named area hit: fall back to the bounding box, top third = head, so a
    // model with sparse hit areas is still fully clickable.
    const bounds = this.model.getBounds();
    if (cx < bounds.x || cx >= bounds.x + bounds.width) return null;
    if (cy < bounds.y || cy >= bounds.y + bounds.height) return null;
    return cy < bounds.y + bounds.height * 0.35 ? "head" : "body";
  }

  hitProfile(cell: number): HitProfile | null {
    if (!this.model || !this.app) return null;
    const renderer = this.app.renderer as PIXI.Renderer;
    if (!renderer.extract) return null;
    const view = this.app.view as HTMLCanvasElement;
    const canvasRect = view.getBoundingClientRect();
    const bounds = this.model.getBounds();
    if (bounds.width <= 0 || bounds.height <= 0) return null;

    // Render the model at one texel per mask cell and read the alpha back: the
    // GPU does the downsampling, and the readback is a few KB, not megabytes.
    const cols = Math.max(1, Math.round(bounds.width / cell));
    const rows = Math.max(1, Math.round(bounds.height / cell));
    let pixels: Uint8Array | Uint8ClampedArray;
    const texture = renderer.generateTexture(this.model, {
      resolution: cols / bounds.width,
      region: bounds,
    });
    try {
      pixels = renderer.extract.pixels(texture);
    } catch (error) {
      void api.logUiError(`character hit profile readback failed: ${String(error)}`);
      return null;
    } finally {
      texture.destroy(true);
    }
    if (pixels.length < cols * rows * 4) return null;

    const solid = new Uint8Array(cols * rows);
    for (let i = 0; i < solid.length; i++) {
      // Anything faintly visible counts; the mask is dilated by the caller.
      if (pixels[i * 4 + 3]! > 16) solid[i] = 1;
    }
    return {
      kind: "cells",
      cell,
      originX: canvasRect.left + bounds.x,
      originY: canvasRect.top + bounds.y,
      cols,
      rows,
      solid,
    };
  }

  destroy(): void {
    this.model?.destroy();
    // Frees the WebGL context; without `true` the canvas leaks across reloads.
    this.app?.destroy(true);
    this.model = undefined;
    this.app = undefined;
  }
}

/**
 * Build the renderer a config asks for. An unknown renderer value falls back to
 * the placeholder — the character still reacts, just without its art.
 */
export function createRenderer(config: CharacterConfig): CharacterRenderer {
  switch (config.renderer) {
    case "live2d":
      return new Live2DRenderer(config);
    case "sprite":
      return new SpriteRenderer(config);
    case "placeholder":
    default:
      return new PlaceholderRenderer();
  }
}
