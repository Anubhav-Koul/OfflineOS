/**
 * The character rendering layer.
 *
 * Phase 3 puts an animated character on the desktop. *What* it does per state is
 * decided on the backend (`ic_widget::character`) and arrives as
 * `character://state`; *how* it is drawn is a `CharacterRenderer`. Three backends
 * are planned behind this one interface:
 *
 *   - `PlaceholderRenderer` — a state-labelled emoji face. No assets, no SDK;
 *     here so the whole pipeline (app state → character state → visible
 *     reaction) works and is verifiable before any art exists.
 *   - `SpriteRenderer` — flat PNG poses (a later slice).
 *   - `Live2DRenderer` — pixi-live2d-display + the Cubism Core (a later slice;
 *     needs the licensed Core and a human render check per CLAUDE.md Phase 3).
 *
 * A character is an asset folder plus a `character.json` ([`CharacterConfig`])
 * mapping each state to renderer-specific animation. Swapping characters or
 * backends is data, not code.
 */

import { Application, Ticker } from "pixi.js";
import { Live2DModel } from "pixi-live2d-display/cubism4";

import type { CharacterState } from "./api";

// pixi-live2d-display drives motions and physics off a shared ticker.
Live2DModel.registerTicker(Ticker);

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
}

/** Fetch and parse a `character.json`. */
export async function loadCharacterConfig(url: string): Promise<CharacterConfig> {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`character config ${url}: HTTP ${response.status}`);
  }
  return (await response.json()) as CharacterConfig;
}

/** The contract every backend implements. */
export interface CharacterRenderer {
  /** Attach to `container`. May load assets, so it can be async. */
  mount(container: HTMLElement): Promise<void> | void;
  /** Show `state`. Called on every `character://state`; must be cheap. */
  setState(state: CharacterState): void;
  /** Detach and free resources. */
  destroy(): void;
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
}

/**
 * A Live2D character, drawn with pixi-live2d-display over the Cubism Core.
 *
 * The Core is a global (`window.Live2DCubismCore`) loaded by `index.html` before
 * the app; its WebAssembly needs `wasm-unsafe-eval` in the CSP. State changes set
 * the mapped expression and, when given, play the mapped motion group.
 */
export class Live2DRenderer implements CharacterRenderer {
  private readonly config: CharacterConfig;
  private app?: Application;
  private model?: Live2DModel;

  constructor(config: CharacterConfig) {
    this.config = config;
  }

  async mount(container: HTMLElement): Promise<void> {
    const modelUrl = this.config.model;
    if (!modelUrl) throw new Error(`character "${this.config.name}" has no model url`);

    const app = new Application({
      resizeTo: container,
      backgroundAlpha: 0,
      antialias: true,
      autoDensity: true,
      resolution: window.devicePixelRatio || 1,
    });
    container.appendChild(app.view as unknown as HTMLCanvasElement);

    const model = await Live2DModel.from(modelUrl);
    app.stage.addChild(model);
    this.app = app;
    this.model = model;
    this.fit(container);
  }

  /** Scale the model to `config.scale` of the container and place it by anchor. */
  private fit(container: HTMLElement): void {
    if (!this.model) return;
    const { clientWidth: w, clientHeight: h } = container;
    const base = Math.min(w / this.model.width, h / this.model.height);
    this.model.scale.set(base * this.config.scale);
    this.model.anchor.set(this.config.anchor.x, this.config.anchor.y);
    this.model.position.set(w * this.config.anchor.x, h * this.config.anchor.y);
  }

  setState(state: CharacterState): void {
    const mapping = this.config.states[state]?.live2d;
    if (!mapping || !this.model) return;
    if (mapping.expression) this.model.expression(mapping.expression);
    if (mapping.motion) void this.model.motion(mapping.motion);
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
 * Build the renderer a config asks for.
 *
 * `sprite` is not built yet, so it falls back to the placeholder — the character
 * still reacts, just without its art.
 */
export function createRenderer(config: CharacterConfig): CharacterRenderer {
  switch (config.renderer) {
    case "live2d":
      return new Live2DRenderer(config);
    case "placeholder":
    case "sprite":
    default:
      return new PlaceholderRenderer();
  }
}
