import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { fileURLToPath, URL } from "node:url";

// Tauri expects a fixed dev port and dist output at ../dist relative to src-tauri.
export default defineConfig({
  root: ".",
  // React + Tailwind sit alongside the existing vanilla UI: surfaces move over
  // one at a time rather than in a single rewrite.
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "es2021",
  },
  server: {
    port: 1420,
    strictPort: true,
  },
  css: { postcss: {} },
  clearScreen: false,
});
