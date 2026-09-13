import { defineConfig } from "vite";

// Tauri expects a fixed dev port and dist output at ../dist relative to src-tauri.
export default defineConfig({
  root: ".",
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
