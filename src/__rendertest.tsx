import { createElement } from "react";
import { mountOverlay } from "@/react/mount";
import { EngagementPanel } from "@/react/EngagementPanel";
import "./styles.css";

const view = {
  dir: "/Users/you/sec/target.example.com",
  findings: [], notes: "", report: "", residual: "",
  files: [{ rel: "deliverables/report.md", bytes: 8192, mtime: Date.now() / 1000 }],
  total_bytes: 8192,
} as any;

const out = (s: string) => { document.title = s; const d = document.createElement("pre"); d.textContent = s; document.body.appendChild(d); };

mountOverlay((done) =>
  createElement("div", { className: "modal-bg" },
    createElement("div", { className: "engagement-shell" },
      createElement(EngagementPanel, {
        view, onOpenFile: async () => "x", onReveal: () => {}, onClose: done,
      }))));

setTimeout(() => {
  const before = !!document.querySelector(".engagement-shell");
  // Esc while a field inside has focus — the case that matters.
  const input = document.querySelector("input") as HTMLInputElement | null;
  input?.focus();
  document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  setTimeout(() => {
    const after = !!document.querySelector(".engagement-shell");
    out(`mounted=${before} closedByEsc=${before && !after} RESULT=${before && !after ? "PASS" : "FAIL"}`);
  }, 60);
}, 400);
