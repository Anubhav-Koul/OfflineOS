/**
 * The canvas window shell.
 *
 * It renders agent-authored HTML/SVG, which is **untrusted** — a prompt-injected
 * agent could emit hostile markup. The isolation is structural, not a sanitizer:
 * the markup goes into an iframe with an **empty `sandbox`** (no scripts, no
 * same-origin, no forms, no navigation, no popups) under a `default-src 'none'`
 * CSP that permits only inline styles and `data:` images/fonts. So a static chart,
 * table, or diagram renders; scripts are inert and every network fetch is blocked.
 *
 * We deliberately do not strip-sanitize (e.g. DOMPurify): the sandbox+CSP already
 * bound what the content can do, and stripping would break legitimate inline SVG.
 * Isolation is the mechanism.
 *
 * The shell itself never sets the iframe via `innerHTML` from the markup — it
 * assigns `iframe.srcdoc`, so the markup is parsed as an isolated document, never
 * as part of this trusted page.
 */
import { render } from "solid-js/web";
import { createSignal, onCleanup, onMount } from "solid-js";

import { canvasContent, onCanvasRender, type CanvasRender } from "./api";
import "./styles.css";

/** The CSP the sandboxed document runs under: no network, inline styles + data:
 *  images only. Injected into the srcdoc so it governs the untrusted content. */
const SANDBOX_CSP =
  "default-src 'none'; img-src data:; style-src 'unsafe-inline' data:; font-src data:; base-uri 'none'; form-action 'none'";

/** Wrap agent markup in a minimal, CSP-guarded document for the iframe. */
function sandboxDoc(markup: string): string {
  return (
    `<!doctype html><html><head><meta charset="utf-8">` +
    `<meta http-equiv="Content-Security-Policy" content="${SANDBOX_CSP}">` +
    `<style>` +
    `html,body{margin:0;padding:16px;box-sizing:border-box;` +
    `font-family:system-ui,-apple-system,Segoe UI,sans-serif;color:#e6e6e6;background:#1b1e24}` +
    `svg,img,table{max-width:100%}` +
    `</style></head><body>${markup}</body></html>`
  );
}

function Canvas() {
  const [empty, setEmpty] = createSignal(true);
  let frame: HTMLIFrameElement | undefined;

  const show = (render: CanvasRender) => {
    if (!frame) return;
    // srcdoc, not innerHTML: the markup becomes a separate sandboxed document,
    // never part of this trusted page.
    frame.srcdoc = sandboxDoc(render.html);
    setEmpty(false);
  };

  onMount(async () => {
    // Read any render that arrived before this shell was listening (first open).
    try {
      const pending = await canvasContent();
      if (pending) show(pending);
    } catch {
      // Non-fatal: a live event will still arrive for the next render.
    }
    const unlisten = await onCanvasRender(show);
    onCleanup(unlisten);
  });

  return (
    <div class="canvas-shell">
      {empty() && <div class="canvas-empty">Nothing to show yet.</div>}
      <iframe
        ref={frame}
        class="canvas-frame"
        classList={{ hidden: empty() }}
        // Empty sandbox = maximal restriction. No allow-scripts, no
        // allow-same-origin: the content cannot reach this page, IPC, or the
        // network. Do not add tokens here without a security review.
        sandbox=""
        title="Rendered content"
      />
    </div>
  );
}

const root = document.getElementById("root");
if (root) render(() => <Canvas />, root);
