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

import type { CharacterState } from "./api";

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
  /** Render scale. */
  scale: number;
  /** Anchor within its box, `0..1` (feet-centre is `{ x: 0.5, y: 1 }`). */
  anchor: { x: number; y: number };
  /** State → animation. Every state is present so a renderer never guesses. */
  states: Record<CharacterState, StateMapping>;
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
 * Build the renderer a config asks for.
 *
 * `sprite` and `live2d` are not built yet, so they fall back to the placeholder
 * rather than failing — the character still reacts, just without its art.
 */
export function createRenderer(config: CharacterConfig): CharacterRenderer {
  switch (config.renderer) {
    case "placeholder":
      return new PlaceholderRenderer();
    default:
      return new PlaceholderRenderer();
  }
}
