import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { fileURLToPath, URL } from "node:url";

// Build rendertest.html on its own, so a migrated React surface can be proved
// to RENDER before it goes anywhere near the app.
//
// Worth having because the failures this migration produces are invisible to
// the type checker: a component called as a plain function (hooks throw, pane
// renders blank), a missing Tailwind base layer (white borders), two transform
// utilities fighting over one property (a switch with no visible knob). Each
// of those shipped once; each would have been caught here.
//
//   npx vite build --config vite.rtest.config.ts
//   # then open dist-rendertest/rendertest.html
export default defineConfig({
  base: "./",
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) },
  },
  build: {
    outDir: "dist-rendertest",
    emptyOutDir: true,
    rollupOptions: {
      input: fileURLToPath(new URL("./rendertest.html", import.meta.url)),
    },
  },
});
