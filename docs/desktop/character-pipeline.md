# The character pipeline (Phase 3)

How the animated companion works, what broke on the way, and what gates a
public release. Written 2026-07-11, at the close of Phase 3.

## Architecture

```
gateway health ─┐
run phase ──────┤  ic_widget::character::derive()   (pure, unit-tested)
listening ──────┤        │ emits character://state on change only
speaking ───────┘        ▼
                 CharacterRenderer (ui/src/character.ts)
                 ├─ Live2DRenderer   pixi-live2d-display + Cubism Core
                 ├─ SpriteRenderer   PNG poses + CSS cheap-puppet
                 └─ PlaceholderRenderer   emoji face, no assets
```

- **States**: `idle · listening · thinking · speaking · concerned · error`.
  Derivation priority is documented on `derive()`. `listening` follows composer
  focus and `speaking` follows reply rendering today (`set_character_signals`);
  Phase 5's wake word and TTS replace both *sources* without touching the seam.
- **A character is data**: an asset folder under `ui/public/characters/<id>/`
  with a `character.json` mapping each state to renderer-specific animation
  (Live2D expression/motion, or a sprite pose). The active character is a
  settings toggle (`settings.json` → `character`), applied by reloading the
  widget window. `manifest.json` lists what is bundled — the assets are
  embedded in the binary, so there is nothing to scan at runtime.
- Because there is no token streaming (see `chat-rendering.md`), `thinking`
  holds until the timeline fetch returns; `speaking` starts when the reply
  renders, and ends on a reading-time estimate until real TTS playback exists.

## The Cubism core/framework mismatch (the Phase 3 compatibility check)

The core shipped in `ui/public/live2d/` is the **Cubism 5 web core** (from
`CubismSdkForWeb-5-r.5`, core version 6.0.1). It moved render orders from
`drawables.renderOrders` to `model.getRenderOrders()` — one combined ordering
covering drawables plus the new "offscreens" — while pixi-live2d-display 0.4.0
bundles the **Cubism 4** framework, which still reads the old field.

The failure was maximally misleading: `renderOrder[0]` threw once on the first
rendered frame, and because a throw inside a Pixi ticker callback ends the rAF
chain (`Ticker._tick` only re-requests the next frame after `update()`
returns), the app's render ticker died permanently — while the model's own
updates kept ticking on the *shared* ticker. One console error, a forever-blank
canvas, and a model that looked alive by every other measure.

Two defenses now exist in `ui/src/character.ts`:

- **`bridgeCoreRenderOrders()`** exposes the new array under the old name on
  every core model. Everything else the Cubism 4 framework touches was verified
  present on the v6 core (parameters, parts, canvasinfo, dynamic-flag utils).
  For a model with no offscreens the bridge is exact. Models that *use*
  offscreens (Ren, Cubism 5.3) get a logged warning — they need the Cubism 5
  framework to render correctly and stay parked until that upgrade.
- **The render loop owns its errors**: the Application's render pass is
  re-added with a try/catch so one bad frame logs once and the loop survives.

**Hiyori (Cubism 4 sample) is the dev model and renders correctly.** Ren
misrenders through the Cubism 4 pipeline (offscreen blending), matching the
CLAUDE.md fallback plan. Retest Ren when pixi-live2d-display ships Cubism 5
framework support (or when we vendor the SDK's own framework).

## Click-through and hit testing

Per-pixel pass-through is split across the IPC boundary
(`ic_widget::hit_test`, `spawn_interaction_watch` in `main.rs`,
`buildHitMask` in `widget.tsx`):

- The **UI** rasterizes what is solid — the chat panel's DOM rect plus the
  character's alpha silhouette (the model rendered at one texel per 8-px cell,
  read back and thresholded) — into a packed bitset, dilated by one cell, and
  pushes it on a 700 ms tick and on panel toggles.
- **Rust** polls the global cursor at ~30 Hz and toggles
  `set_ignore_cursor_events` by testing the mask. The webview receives no mouse
  events at all while click-through, so the decision to become clickable again
  cannot live in the webview.
- The same poll emits `cursor://pos` (eye tracking follows the cursor even over
  click-through pixels) and, at 1 Hz, `character://active` — animation pauses
  while a fullscreen app is foreground, so the idle character never competes
  with a game or llama.cpp for the GPU. Both tickers are capped at 30 fps.
- No mask yet (UI still booting) = fully interactive. Failing interactive costs
  a stray click on empty space; failing click-through would strand a window
  nothing can ever click again.

Click routing: the model's own hit areas first (`Head`/`Body`; Hiyori only has
`Body`), then a bounding-box fallback (top third = head). Head toggles the chat
panel; body starts an OS window drag.

## Known follow-ups

- **The `Texture.fromURL` <img>→canvas override may be removable.** The
  "WebView2 uploads blank textures from HTMLImageElement" diagnosis was made
  while the dead render ticker masked everything. One smoke run without the
  override settles it.
- **Ren / Cubism 5 framework** — see above.
- **Phase 5** wires wake-word/VAD into `listening`, TTS playback into
  `speaking`, and real playback amplitude into the lip-sync parameter (the
  test-tone stub in `Live2DRenderer.patchLipSync` marks the exact seam).

## Licensing gates before any public release

Tracked here so Phase 6 packaging cannot forget them:

1. **Live2D Cubism Core / SDK** — redistribution is governed by the Live2D
   Proprietary Software License Agreement; releases above the "small-scale
   enterprise" revenue threshold need a paid publication license. Verify our
   tier before shipping the core in an MSI.
2. **Hiyori** (official sample) — Live2D Free Material License: permitted for
   general/small-scale use; re-verify the current terms and credit requirements
   at release time.
3. **Ren Foster** (official sample, "PRO") — Live2D Free Material License;
   same re-verification, and it must not ship while it misrenders anyway.
4. **`SpriteRenderer` art** — user-supplied/dev-only images must never be
   bundled into a public release. Nothing ships in `characters/*/sprite/`.
5. Any commissioned or marketplace model — check its redistribution terms
   before bundling (CLAUDE.md Phase 3.7).
