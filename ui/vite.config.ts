import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

// Two entry points: the always-on-top widget and the dashboard window. Tauri
// loads each by path, so this is a plain multi-page build.
export default defineConfig({
  plugins: [solid()],
  // Tauri controls the dev server port; failing loudly beats silently moving to
  // 1421 and leaving the webview pointed at nothing.
  server: { port: 1420, strictPort: true },
  build: {
    target: "chrome110", // WebView2 on Win10+; no need to transpile further
    rollupOptions: {
      input: {
        widget: "index.html",
        dashboard: "dashboard.html",
        canvas: "canvas.html",
      },
    },
  },
});
