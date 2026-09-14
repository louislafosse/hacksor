import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { marked } from "marked";
import { markedHighlight } from "marked-highlight";
import hljs from "highlight.js";
import markedKatex from "marked-katex-extension";
import mermaid from "mermaid";
import "katex/dist/katex.min.css";

marked.setOptions({ breaks: true, gfm: true });
// Syntax highlighting (highlight.js) — skip fenced ```mermaid so those survive
// as raw text for the diagram renderer below.
marked.use(
  markedHighlight({
    langPrefix: "hljs language-",
    highlight(code, lang) {
      if (lang === "mermaid") return code;
      if (lang && hljs.getLanguage(lang)) {
        try { return hljs.highlight(code, { language: lang }).value; } catch { /* fall through */ }
      }
      try { return hljs.highlightAuto(code).value; } catch { return code; }
    },
  }),
);
// KaTeX math ($…$ and $$…$$).
marked.use(markedKatex({ throwOnError: false, nonStandard: true }));
mermaid.initialize({ startOnLoad: false, securityLevel: "strict" });

// Render any ```mermaid blocks inside a finalized message container to SVG.
let mermaidSeq = 0;
function enhanceMermaid(container: HTMLElement) {
  const blocks = container.querySelectorAll("code.language-mermaid");
  blocks.forEach((code) => {
    const pre = code.closest("pre") ?? code;
    const src = (code.textContent || "").trim();
    if (!src) return;
    const id = `mmd-${++mermaidSeq}`;
    mermaid.render(id, src).then(({ svg }) => {
      const box = document.createElement("div");
      box.className = "mermaid-diagram";
      box.innerHTML = svg;
      pre.replaceWith(box);
    }).catch(() => { /* leave the code block as-is on parse error */ });
  });
}

// Add a Copy button to each code block in a rendered answer (skips mermaid,
// which becomes a diagram). Idempotent — safe to call after re-render/restore.
function enhanceCodeBlocks(container: HTMLElement) {
  container.querySelectorAll("pre").forEach((pre) => {
    if (pre.querySelector(":scope > .code-copy")) return;   // already added
    if (pre.querySelector("code.language-mermaid")) return; // rendered as a diagram
    const code = pre.querySelector("code");
    const btn = document.createElement("button");
    btn.className = "code-copy";
    btn.type = "button";
    btn.title = "Copy code";
    btn.textContent = "Copy";
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      copyWithFeedback((code?.textContent ?? pre.textContent ?? "").replace(/\n$/, ""), btn);
    });
    pre.appendChild(btn);
  });
}

type ProviderInfo = { id: string; display: string };
type Model = {
  id: string;
  display_name: string;
  description: string;
  context_length: number | null;
  supports_vision: boolean;
  prompt_price: string | null;
  reasoning_effort: string;
};
type SettingsView = {
  provider: string;
  has_openrouter_key: boolean;
  has_vercel_key: boolean;
  personality: string | null;
  custom_instructions: string | null;
  working_dir: string;
  kali_mode: boolean;
  runtime: string;
  services_mode?: string;
  has_cloudflare?: boolean;
  cloudflare_email?: string | null;
  cloudflare_account_id?: string | null;
};
type Notif = { method: string; params: any };

type ResizeDir =
  | "North" | "South" | "East" | "West"
  | "NorthEast" | "NorthWest" | "SouthEast" | "SouthWest";
const RESIZE_DIRS: Record<string, ResizeDir> = {
  n: "North", s: "South", e: "East", w: "West",
  ne: "NorthEast", nw: "NorthWest", se: "SouthEast", sw: "SouthWest",
};

const GREETINGS = [
  "What should we hack?",
  "Got an idea?",
  "What are we testing today?",
  "Where do we start?",
  "What's our target today?",
  "What's on the scope today?",
  "What are we exploiting today?",
  "Ready to find some vulns?",
];
const SUGGESTIONS = [
  { icon: "🛰", label: "Recon a target", text: "Run reconnaissance on the target: " },
  { icon: "🔎", label: "Scan for vulns", text: "Scan this host for vulnerabilities and prioritize findings: " },
  { icon: "🧬", label: "Review a repo", text: "Audit this repository for security issues: " },
  { icon: "💥", label: "Write a PoC", text: "Write a working proof-of-concept exploit for: " },
];

type ItemBlock = {
  root: HTMLElement;
  buffer: string;
  setFinal?: (item: any) => void;
  appendDelta?: (s: string) => void;
};

interface Session {
  id: string;
  title: string;
  threadId: string | null;
  // The provider the current codex thread was CREATED under. codex pins a
  // thread to its creation provider (turn/start & thread/resume ignore
  // modelProvider — verified), so a provider switch must start a new thread.
  threadProvider?: string;
  workingDir: string;
  turnsEl: HTMLDivElement;
  blocks: Map<string, ItemBlock>;
  planEl: HTMLUListElement | null;
  running: boolean;
  starting: boolean;
  started: boolean;
  archived: boolean;
  unread: boolean;
  resumed?: boolean;
  // Per-chat composer settings + draft.
  provider: string;
  model: string;
  effort: string;
  mode: "agent" | "ask";
  permission: "full_access" | "ask_approval" | "auto_review" | "read_only";
  auto: boolean;
  role: "default" | "task" | "validate";
  draft: string;
  escalation: number;
  recency: number;
  queue: string[];
  cmdSig?: string;
  cmdRepeat?: number;
  // Per-turn: did the model emit ANY output (text, reasoning, or a command)?
  // Used to detect a fully-empty completion (e.g. Anthropic models decline the
  // security persona and return nothing) so the UI surfaces it, without a false
  // positive on a turn that ran commands but produced no final message.
  sawOutputThisTurn?: boolean;
  // Per-turn timing/selection, for the per-answer Info popover. turnStartMs is
  // stamped on turn/started; the answer element is tagged so Info can correlate
  // it to the ocx usage record(s) in that window.
  turnStartMs?: number;
  turnModel?: string;
  lastAnswerEl?: HTMLElement;
  // Last model Auto used in this chat, so we only note a switch when it changes.
  autoModel?: string;
  thinkingEl?: HTMLElement;
  thinkTimer?: number;
  lastText?: string;
  lastImages?: string[];
  // Shell-style prompt history for this chat (every message you've sent, oldest
  // first). Up/Down in the composer walk it. Seeded from the transcript on resume.
  sentHistory: string[];
}

const app = document.getElementById("app")!;

const state = {
  providers: [] as ProviderInfo[],
  provider: "openrouter",
  models: [] as Model[],
  model: "",
  mode: "agent" as "agent" | "ask",
  auto: false,
  effort: "high",
  permission: "full_access" as "full_access" | "ask_approval" | "auto_review" | "read_only",
  keys: { openrouter: false, vercel: false },
  personality: null as string | null,
  customInstructions: "",
  workingDir: "",
  kaliMode: false,
  runtime: "host",
  sessions: [] as Session[],
  activeSessionId: "",
  attachments: [] as { path: string; name: string; isImage: boolean }[],
};

const IMAGE_RE = /\.(png|jpe?g|gif|webp|bmp|svg|avif)$/i;

// Matching line-icon set (archive box, restore, trash).
const ARCHIVE_ICON =
  `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="4" rx="1"/><path d="M5 8v11a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1V8"/><path d="M10 12h4"/></svg>`;
const RESTORE_ICON =
  `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="4" rx="1"/><path d="M5 8v11a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1V8"/><path d="M12 18v-6"/><path d="M9.5 14.5 12 12l2.5 2.5"/></svg>`;
const DELETE_ICON =
  `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18"/><path d="M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2"/><path d="M6 6v14a1 1 0 0 0 1 1h10a1 1 0 0 0 1-1V6"/><path d="M10 11v6M14 11v6"/></svg>`;
const PIN_ICON =
  `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="12" x2="12" y1="17" y2="22"/><path d="M5 17h14v-1.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V6h1a2 2 0 0 0 0-4H8a2 2 0 0 0 0 4h1v4.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24Z"/></svg>`;
const PINNED_ICON =
  `<svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="12" x2="12" y1="17" y2="22"/><path d="M5 17h14v-1.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V6h1a2 2 0 0 0 0-4H8a2 2 0 0 0 0 4h1v4.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24Z"/></svg>`;
// Sidebar action icons (Lucide-style stroke, matching the archive/pin set).
const IC = (path: string) =>
  `<svg width="17" height="17" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round">${path}</svg>`;
const SEARCH_ICON = IC('<circle cx="11" cy="11" r="7"/><path d="m21 21-4.3-4.3"/>');
const ATTACH_ICON = IC('<path d="m21.44 11.05-9.19 9.19a6 6 0 0 1-8.49-8.49l8.57-8.57A4 4 0 1 1 18 8.84l-8.59 8.57a2 2 0 0 1-2.83-2.83l8.49-8.48"/>');
const NOTES_ICON = IC('<path d="M15 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M15 3v5h5"/><path d="M9 13h6"/><path d="M9 17h4"/>');
const SETTINGS_ICON = IC('<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/>');

// Persisted archive / permanently-deleted thread ids (survive restart, since
// Recents is reloaded from disk).
function loadSet(key: string): Set<string> {
  try { return new Set(JSON.parse(localStorage.getItem(key) || "[]")); } catch { return new Set(); }
}
function saveSet(key: string, set: Set<string>) {
  try { localStorage.setItem(key, JSON.stringify([...set])); } catch {}
}
const archivedIds = loadSet("hacksor-archived");
const hiddenIds = loadSet("hacksor-hidden");
const pinnedIds = loadSet("hacksor-pinned"); // by threadId
function togglePin(tid: string | null | undefined) {
  if (!tid) return;
  if (pinnedIds.has(tid)) pinnedIds.delete(tid); else pinnedIds.add(tid);
  saveSet("hacksor-pinned", pinnedIds);
  renderSidebar();
}

// Persist the set of open chats (thread id + title + settings) so they survive
// a restart; their transcripts are resumed lazily on first select.
function persistOpenChats() {
  try {
    const list = state.sessions
      .filter((s) => s.started && !s.archived && s.threadId)
      .map((s) => ({
        threadId: s.threadId, title: s.title, workingDir: s.workingDir,
        provider: s.provider, model: s.model, effort: s.effort,
        mode: s.mode, permission: s.permission, auto: s.auto, role: s.role,
      }));
    localStorage.setItem("hacksor-open-chats", JSON.stringify(list));
  } catch {}
}
function loadOpenChats(): any[] {
  try { return JSON.parse(localStorage.getItem("hacksor-open-chats") || "[]"); } catch { return []; }
}

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K, cls?: string, html?: string,
): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (html !== undefined) e.innerHTML = html;
  return e;
}
function escapeHtml(s: string): string {
  return (s ?? "").replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]!),
  );
}
function keyBadge(saved: boolean): string {
  return saved
    ? '<span class="saved-badge">✓ Saved</span>'
    : '<span class="unset-badge">Not set</span>';
}
function shortPath(p: string): string {
  if (!p) return "";
  const parts = p.split("/");
  return parts.length > 2 ? "…/" + parts.slice(-2).join("/") : p;
}
function uid(): string {
  return Math.random().toString(36).slice(2, 10);
}

// Persist composer preferences (provider/model/effort/mode/permission/auto).
function savePrefs() {
  try {
    localStorage.setItem("hacksor-prefs", JSON.stringify({
      provider: state.provider, model: state.model, auto: state.auto,
      effort: state.effort, mode: state.mode, permission: state.permission,
    }));
  } catch {}
}
function loadPrefs(): any {
  try { const raw = localStorage.getItem("hacksor-prefs"); if (raw) return JSON.parse(raw); } catch {}
  return null;
}
// Per-chat draft persistence, keyed by the chat's (stable) thread id so an
// unsent draft survives closing the app and reopening that chat from Recents.
function persistDraft(session: Session) {
  if (!session.threadId) return;
  try {
    const raw = localStorage.getItem("hacksor-drafts");
    const map = raw ? JSON.parse(raw) : {};
    if (session.draft) map[session.threadId] = session.draft;
    else delete map[session.threadId];
    localStorage.setItem("hacksor-drafts", JSON.stringify(map));
  } catch {}
}
function loadDraftFor(threadId: string): string {
  try {
    const raw = localStorage.getItem("hacksor-drafts");
    const map = raw ? JSON.parse(raw) : {};
    return map[threadId] || "";
  } catch { return ""; }
}

function providerDisplay(id: string): string {
  return state.providers.find((p) => p.id === id)?.display ?? id;
}

// The Codex-level backend a composer provider maps to. All three (OpenRouter,
// Vercel, OpenCodex) now route through the local OpenCodex proxy — the upstream
// is chosen by the model-id prefix — so switching between them is just a model
// change on the same thread, never a new thread.
function codexBackend(_providerId: string): string {
  return "opencodex";
}

// Copy the current UI selection into the active chat (per-chat settings).
function syncActiveSession() {
  const s = state.sessions.find((x) => x.id === state.activeSessionId);
  if (!s) return;
  s.provider = state.provider;
  s.model = state.model;
  s.effort = state.effort;
  s.mode = state.mode;
  s.permission = state.permission;
  s.auto = state.auto;
}

function applyTheme(theme: string) {
  const dark = theme !== "light";
  document.documentElement.classList.toggle("dark", dark);
  try { localStorage.setItem("hacksor-theme", dark ? "dark" : "light"); } catch {}
}
applyTheme((() => {
  try { return localStorage.getItem("hacksor-theme") || "dark"; } catch { return "dark"; }
})());

// ---------------------------------------------------------------- layout

app.innerHTML = `
  <aside class="sidebar" id="sidebar">
    <div class="top">
      <div class="brand">Hacksor</div>
      <button class="new-chat" id="newchat">
        <svg class="nc-icon" width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 20h9"/><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4Z"/></svg>
        New chat
      </button>
    </div>
    <div class="sidebar-scroll">
      <div class="section-label">Recents</div>
      <div class="chat-list" id="sesslist"></div>
      <div class="chat-list" id="recentslist"></div>
      <div class="section-label archive-head" id="archlabel" hidden>Archive</div>
      <div class="chat-list arch" id="archlist"></div>
    </div>
    <div class="bottom">
      <button class="side-btn" id="searchbtn"><span class="sb-ic">${SEARCH_ICON}</span> Search chats</button>
      <button class="side-btn" id="notesbtn"><span class="sb-ic">${NOTES_ICON}</span> Notes</button>
      <button class="side-btn" id="settingsbtn"><span class="sb-ic">${SETTINGS_ICON}</span> Settings</button>
    </div>
  </aside>
  <div class="main">
    <div class="topbar" data-tauri-drag-region>
      <button class="icon-btn" id="sidetoggle" title="Toggle sidebar (Ctrl+S)">☰</button>
      <div class="grow" data-tauri-drag-region></div>
      <div class="wincontrols">
        <button class="icon-btn win-btn" id="win-min" title="Minimize">─</button>
        <button class="icon-btn win-btn" id="win-max" title="Maximize">□</button>
        <button class="icon-btn win-btn win-close" id="win-close" title="Close">✕</button>
      </div>
    </div>
    <div class="scroll" id="scroll">
      <div class="col"><div id="turnshost"></div></div>
    </div>
    <button class="jump-latest" id="jumplatest" hidden title="Jump to latest">↓ Latest</button>
    <div class="composer-wrap">
      <div class="queue" id="queue" hidden></div>
      <div class="composer" id="composer">
        <div class="attachments" id="attachments" hidden></div>
        <textarea id="input" rows="1" placeholder="Ask Hacksor to …"></textarea>
        <div class="toolbar">
          <div class="pill-toggle" id="mode">
            <button data-v="agent" class="active">Agent</button>
            <button data-v="ask">Ask</button>
          </div>
          <button class="tchip" id="modelchip" title="Model & provider">model</button>
          <button class="tchip" id="effortchip" title="Reasoning effort">High</button>
          <button class="tchip" id="permchip" title="Approval mode">Full access</button>
          <div class="grow"></div>
          <button class="icon-btn attach" id="attach" title="Attach files">${ATTACH_ICON}</button>
          <button class="send" id="send" title="Send">↑</button>
        </div>
        <div class="drop-overlay" id="dropoverlay" hidden>${ATTACH_ICON} Drop files to attach</div>
      </div>
      <div class="hint" id="hint"></div>
    </div>
  </div>
  <div class="tooldrawer" id="tooldrawer" hidden>
    <div class="td-head">
      <span class="td-icon" id="td-icon">❯</span>
      <span class="td-title" id="td-title"></span>
      <button class="icon-btn" id="td-copy" title="Copy">⧉</button>
      <button class="icon-btn" id="td-close" title="Close">✕</button>
    </div>
    <pre class="td-body" id="td-body"></pre>
  </div>
`;

const sidebar = document.getElementById("sidebar") as HTMLElement;
const turnsHost = document.getElementById("turnshost") as HTMLDivElement;
const scrollEl = document.getElementById("scroll") as HTMLDivElement;
const jumpBtn = document.getElementById("jumplatest") as HTMLButtonElement;
let stickToBottom = true;
let programmaticScroll = false; // set while WE scroll, so our own scroll isn't read as user intent
let lastScrollTop = 0;
jumpBtn.addEventListener("click", () => { stickToBottom = true; scrollToBottom(true); });
scrollEl.addEventListener("scroll", () => {
  // Ignore the scroll event caused by our own scrollToBottom() — otherwise the
  // per-frame auto-scroll during streaming would keep re-asserting "at bottom"
  // and fight the user trying to scroll up.
  if (programmaticScroll) { programmaticScroll = false; lastScrollTop = scrollEl.scrollTop; return; }
  const goingUp = scrollEl.scrollTop < lastScrollTop - 1;
  lastScrollTop = scrollEl.scrollTop;
  const atBottom = scrollEl.scrollHeight - scrollEl.scrollTop - scrollEl.clientHeight < 40;
  // Any upward move means the user wants to read: stop following immediately.
  // We only resume following once they return to the very bottom.
  if (goingUp) stickToBottom = false;
  else if (atBottom) stickToBottom = true;
  jumpBtn.hidden = stickToBottom;
});
const sessList = document.getElementById("sesslist") as HTMLDivElement;
const modelChip = document.getElementById("modelchip") as HTMLButtonElement;
const effortChip = document.getElementById("effortchip") as HTMLButtonElement;
const permChip = document.getElementById("permchip") as HTMLButtonElement;
const input = document.getElementById("input") as HTMLTextAreaElement;

const PERMS: { v: "full_access" | "ask_approval" | "auto_review" | "read_only"; label: string }[] = [
  { v: "full_access", label: "Full access" },
  { v: "auto_review", label: "Auto-review" },
  { v: "ask_approval", label: "Ask approval" },
  { v: "read_only", label: "Read only" },
];

// Normalized reasoning-effort ladder. "auto" means "don't override" — the
// effort is derived from the model, which keeps it valid across models that
// support different subsets. Explicit levels map to the harness effort values.
const EFFORTS: { v: string; label: string }[] = [
  { v: "minimal", label: "Minimal" },
  { v: "low", label: "Low" },
  { v: "medium", label: "Medium" },
  { v: "high", label: "High" },
  { v: "xhigh", label: "Max" },
];
const sendBtn = document.getElementById("send") as HTMLButtonElement;
const hint = document.getElementById("hint") as HTMLDivElement;

// Runtime provisioning state: while the Docker runtime is pulling/building/
// starting, the composer is blocked and the full build log is accumulated so the
// user can open it from the banner.
let runtimeBusy = false;
let runtimeLog = "";
let buildDrawerOpen = false;
let rtBanner: HTMLElement | null = null;

// Create the runtime banner element once (message row + build-log button +
// progress bar); return it. Both the provisioning-event listener and the
// error handler use it, so a Docker-missing error (which is returned before any
// runtime event fires) still gets a visible banner.
function ensureRuntimeBanner(): HTMLElement {
  if (!rtBanner) {
    rtBanner = el("div", "runtime-banner");
    rtBanner.innerHTML = `<div class="rt-row"><span class="rt-msg"></span><button class="rt-view" type="button">View build output</button><span class="rt-pct"></span></div><div class="rt-bar"><div class="rt-fill"></div></div>`;
    rtBanner.querySelector(".rt-view")!.addEventListener("click", showBuildLog);
    app.appendChild(rtBanner);
  }
  return rtBanner;
}

// Block/unblock the composer and tell the user to wait during a runtime build.
function applyRuntimeLock() {
  const composer = document.getElementById("composer");
  if (!composer) return;
  input.disabled = runtimeBusy;
  sendBtn.disabled = runtimeBusy;
  composer.classList.toggle("locked", runtimeBusy);
  if (runtimeBusy) {
    if (input.dataset.ph === undefined) input.dataset.ph = input.placeholder;
    input.placeholder = "Preparing the runtime environment — please wait until the build finishes…";
  } else if (input.dataset.ph !== undefined) {
    input.placeholder = input.dataset.ph;
    delete input.dataset.ph;
  }
}

// Open the right-side drawer showing the live Docker build output.
function showBuildLog() {
  openToolDrawer("🐳", "Runtime build output", runtimeLog || "Waiting for build output…");
  buildDrawerOpen = true;
  const body = document.getElementById("td-body") as HTMLElement;
  body.scrollTop = body.scrollHeight;
}

function toggleSidebar() {
  sidebar.classList.toggle("collapsed");
}

document.getElementById("sidetoggle")!.addEventListener("click", toggleSidebar);
document.getElementById("win-min")!.addEventListener("click", () => getCurrentWindow().minimize().catch(() => {}));
document.getElementById("win-max")!.addEventListener("click", () => getCurrentWindow().toggleMaximize().catch(() => {}));
document.getElementById("win-close")!.addEventListener("click", () => getCurrentWindow().close().catch(() => {}));
for (const dir of ["n", "s", "e", "w", "ne", "nw", "se", "sw"] as const) {
  const grip = el("div", `resize-grip grip-${dir}`);
  grip.addEventListener("mousedown", (e) => {
    e.preventDefault();
    getCurrentWindow().startResizeDragging(RESIZE_DIRS[dir] as any).catch(() => {});
  });
  app.appendChild(grip);
}

// Ctrl+S toggles the sidebar; Ctrl/⌘+K opens chat search.
window.addEventListener("keydown", (e) => {
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s") {
    e.preventDefault();
    toggleSidebar();
  }
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
    e.preventDefault();
    openSearch();
  }
});

document.getElementById("newchat")!.addEventListener("click", () => newSession());
document.getElementById("settingsbtn")!.addEventListener("click", () => openSettings());
document.getElementById("notesbtn")!.addEventListener("click", () => openNotes());
document.getElementById("searchbtn")!.addEventListener("click", () => openSearch());
document.getElementById("td-close")!.addEventListener("click", closeToolDrawer);
document.getElementById("td-copy")!.addEventListener("click", () => {
  navigator.clipboard?.writeText((document.getElementById("td-body") as HTMLElement).textContent || "").catch(() => {});
});
document.getElementById("attach")!.addEventListener("click", async () => {
  const paths = await invoke<string[]>("pick_files").catch(() => [] as string[]);
  addAttachments(paths);
});

// ---- file attachments (attach button + drag & drop) ----
const attachmentsEl = document.getElementById("attachments") as HTMLDivElement;
const dropOverlay = document.getElementById("dropoverlay") as HTMLDivElement;

function addAttachments(paths: string[]) {
  for (const p of paths) {
    if (!p || state.attachments.some((a) => a.path === p)) continue;
    const name = p.split("/").pop() || p;
    state.attachments.push({ path: p, name, isImage: IMAGE_RE.test(p) });
  }
  renderAttachments();
}
function removeAttachment(path: string) {
  state.attachments = state.attachments.filter((a) => a.path !== path);
  renderAttachments();
}
function renderAttachments() {
  attachmentsEl.innerHTML = "";
  attachmentsEl.hidden = state.attachments.length === 0;
  for (const a of state.attachments) {
    const chip = el("div", "att-chip");
    chip.innerHTML = `<span>${a.isImage ? "🖼" : "📄"}</span><span class="att-name">${escapeHtml(a.name)}</span>`;
    const x = el("button", "att-x", "✕");
    x.onclick = () => removeAttachment(a.path);
    chip.appendChild(x);
    attachmentsEl.appendChild(chip);
  }
}

getCurrentWindow().onDragDropEvent((e) => {
  const t = e.payload.type;
  if (t === "enter" || t === "over") dropOverlay.hidden = false;
  else if (t === "leave") dropOverlay.hidden = true;
  else if (t === "drop") {
    dropOverlay.hidden = true;
    addAttachments((e.payload as any).paths ?? []);
  }
}).catch(() => {});

document.getElementById("mode")!.addEventListener("click", (e) => {
  const t = (e.target as HTMLElement).closest("button");
  if (!t) return;
  state.mode = t.dataset.v as any;
  document.querySelectorAll("#mode button").forEach((b) => b.classList.remove("active"));
  t.classList.add("active");
  permChip.hidden = state.mode === "ask";
  updateHint();
  syncActiveSession();
  savePrefs();
});
modelChip.addEventListener("click", openModelMenu);
effortChip.addEventListener("click", openEffortMenu);
permChip.addEventListener("click", openPermMenu);

function updateChips() {
  modelChip.textContent = state.auto ? "✦ Auto" : (state.model || "model");
  const prov = state.providers.find((p) => p.id === state.provider)?.display ?? "";
  modelChip.title = `${prov}${state.auto ? "" : " · " + state.model}`;
  // Effort is auto-managed while Auto is on, so hide the (non-interactive) chip.
  effortChip.hidden = state.auto;
  effortChip.textContent = EFFORTS.find((e) => e.v === state.effort)?.label ?? "High";
  permChip.textContent = PERMS.find((p) => p.v === state.permission)?.label ?? "Full access";
  syncActiveSession();
  savePrefs();
}

function openEffortMenu() {
  if (state.auto) return; // effort is auto-managed while Auto is on
  const panel = el("div", "menu-list");
  for (const e of EFFORTS) {
    const row = el("button", "menu-item ctx" + (e.v === state.effort ? " sel" : ""), e.label);
    row.onclick = () => { state.effort = e.v; updateChips(); closeMenu(); };
    panel.appendChild(row);
  }
  popup(effortChip, panel);
}

// ---- lightweight floating menu (no native selects) ----
let openPanel: HTMLElement | null = null;
function closeMenu() {
  if (openPanel) { openPanel.remove(); openPanel = null; }
  document.removeEventListener("mousedown", onDocDown, true);
  document.removeEventListener("keydown", onMenuEsc, true);
}
function onDocDown(e: MouseEvent) {
  if (openPanel && !openPanel.contains(e.target as Node)) closeMenu();
}
function onMenuEsc(e: KeyboardEvent) {
  if (e.key === "Escape") { e.preventDefault(); closeMenu(); }
}
function popup(anchor: HTMLElement, panel: HTMLElement) {
  closeMenu();
  panel.classList.add("menu");
  document.body.appendChild(panel);
  const r = anchor.getBoundingClientRect();
  const pw = panel.offsetWidth;
  let left = r.left;
  if (left + pw > window.innerWidth - 12) left = window.innerWidth - pw - 12;
  panel.style.left = Math.max(12, left) + "px";
  panel.style.bottom = window.innerHeight - r.top + 8 + "px";
  openPanel = panel;
  setTimeout(() => {
    document.addEventListener("mousedown", onDocDown, true);
    document.addEventListener("keydown", onMenuEsc, true);
  }, 0);
}

function openPermMenu() {
  const panel = el("div", "menu-list");
  for (const p of PERMS) {
    const row = el("button", "menu-item" + (p.v === state.permission ? " sel" : ""), p.label);
    row.onclick = () => { state.permission = p.v; updateChips(); updateHint(); closeMenu(); };
    panel.appendChild(row);
  }
  popup(permChip, panel);
}

const PROVIDER_SUB: Record<string, string> = {
  openrouter: "400+ models · one key",
  vercel: "Vercel's model gateway",
  opencodex: "Route to any provider via OpenCodex",
};
function openModelMenu() {
  const panel = el("div", "menu-model");
  // Provider chooser — a vertical list (long names never fit a pill row).
  panel.appendChild(el("div", "menu-heading", "Provider"));
  const seg = el("div", "prov-seg");
  for (const p of state.providers) {
    const active = p.id === state.provider;
    const b = el("button", "prov-opt" + (active ? " active" : ""));
    b.innerHTML =
      `<span class="po-radio">${active ? "●" : "○"}</span>` +
      `<span class="po-text"><span class="po-name">${escapeHtml(p.display)}</span>` +
      `<span class="po-sub">${escapeHtml(PROVIDER_SUB[p.id] ?? "")}</span></span>`;
    b.onclick = async () => {
      if (active) return;
      state.provider = p.id;
      await invoke("save_settings", { args: { provider: state.provider } });
      await loadModels();
      openModelMenu();
    };
    seg.appendChild(b);
  }
  panel.appendChild(seg);

  // auto row
  const autoRow = el("button", "menu-item auto-row" + (state.auto ? " sel" : ""),
    `✦ Auto <span class="hintmini">pick the best model per task</span>`);
  autoRow.onclick = () => { state.auto = true; updateChips(); updateHint(); closeMenu(); };
  panel.appendChild(autoRow);

  // search + list
  const search = el("input", "menu-search") as HTMLInputElement;
  search.placeholder = "Search models…";
  panel.appendChild(search);
  const list = el("div", "menu-scroll");
  panel.appendChild(list);

  const renderList = (q: string) => {
    list.innerHTML = "";
    const ql = q.toLowerCase();
    const rows = state.models.filter((m) => !ql || m.id.toLowerCase().includes(ql) || m.display_name.toLowerCase().includes(ql)).slice(0, 200);
    if (!rows.length) { list.appendChild(el("div", "menu-empty", state.models.length ? "No match" : "No models — add a key in Settings")); return; }
    for (const m of rows) {
      const row = el("button", "menu-item" + (!state.auto && m.id === state.model ? " sel" : ""));
      row.innerHTML = `<span class="mi-name">${escapeHtml(m.display_name)}</span><span class="mi-id">${escapeHtml(m.id)}</span>`;
      row.onclick = () => { state.auto = false; state.model = m.id; updateChips(); updateHint(); closeMenu(); };
      list.appendChild(row);
    }
  };
  renderList("");
  search.addEventListener("input", () => renderList(search.value));
  popup(modelChip, panel);
  setTimeout(() => search.focus(), 0);
}
// Shell-style prompt recall: histIdx points into the active session's
// sentHistory (-1 = not navigating); histStash holds whatever was in the box
// before recall started, restored when you arrow back down past the newest.
let histIdx = -1;
let histStash = "";
function autosizeInput() {
  input.style.height = "auto";
  input.style.height = Math.min(input.scrollHeight, 220) + "px";
}
// Set the composer text during history recall WITHOUT resetting the cursor
// (dispatching a real "input" event would clear histIdx).
function setComposer(text: string) {
  input.value = text;
  autosizeInput();
  const s = activeSession();
  if (s) { s.draft = text; persistDraft(s); }
  input.setSelectionRange(text.length, text.length);
}
input.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); onSend(); return; }
  const s = activeSession();
  if (e.key === "ArrowUp") {
    // Empty composer, not yet recalling: a queued (not-yet-sent) steer message
    // takes priority, then fall through to prompt history.
    if (histIdx === -1 && input.value === "" && s && s.queue.length) {
      e.preventDefault();
      setComposer(s.queue.pop()!);
      renderQueue(s);
      return;
    }
    // Walk back through prompt history. Engage when the box is empty, or keep
    // walking if we're already recalling (so repeated ArrowUp shows older ones).
    if (s && s.sentHistory.length && (input.value === "" || histIdx >= 0)) {
      e.preventDefault();
      if (histIdx === -1) { histStash = input.value; histIdx = s.sentHistory.length; }
      if (histIdx > 0) { histIdx--; setComposer(s.sentHistory[histIdx]); }
      return;
    }
  }
  if (e.key === "ArrowDown" && histIdx >= 0 && s) {
    e.preventDefault();
    if (histIdx < s.sentHistory.length - 1) { histIdx++; setComposer(s.sentHistory[histIdx]); }
    else { histIdx = -1; setComposer(histStash); } // past the newest → restore draft
    return;
  }
});
input.addEventListener("input", () => {
  histIdx = -1; // typing exits history recall
  autosizeInput();
  const s = activeSession();
  if (s) { s.draft = input.value; persistDraft(s); }
});
sendBtn.addEventListener("click", onSend);

// ---------------------------------------------------------------- sessions

function activeSession(): Session | undefined {
  return state.sessions.find((s) => s.id === state.activeSessionId);
}
function sessionByThread(threadId?: string): Session | undefined {
  if (!threadId) return activeSession();
  return state.sessions.find((s) => s.threadId === threadId);
}

function currentDir(): string {
  return activeSession()?.workingDir || state.workingDir;
}

function updateHint() {
  const prov = state.providers.find((p) => p.id === state.provider)?.display ?? state.provider;
  const perm = state.mode === "ask" ? "read-only" : state.permission.replace("_", " ");
  const dir = currentDir();
  hint.textContent = `${prov} · ${state.mode} · ${perm} · runs in ${dir || "?"}`;
}

function makeSession(): Session {
  const turnsEl = el("div", "turns");
  turnsEl.hidden = true;
  turnsHost.appendChild(turnsEl);
  return {
    id: uid(), title: "New chat", threadId: null, workingDir: state.workingDir,
    turnsEl, blocks: new Map(), planEl: null,
    running: false, starting: false, started: false, archived: false, unread: false,
    // New chats inherit the current selection as their starting point.
    provider: state.provider, model: state.model, effort: state.effort,
    mode: state.mode, permission: state.permission, auto: state.auto, draft: "",
    role: "default", escalation: 0, recency: Date.now(), queue: [], sentHistory: [],
  };
}

function newSession(role: "default" | "task" | "validate" = "default") {
  const s = makeSession();
  s.role = role;
  state.sessions.push(s);
  selectSession(s.id);
  renderSidebar();
  const dom = sessionItemEl(s.id);
  if (dom) { dom.classList.add("item-enter"); setTimeout(() => dom.classList.remove("item-enter"), 220); }
  input.focus();
}


function selectSession(id: string) {
  state.activeSessionId = id;
  stickToBottom = true; // opening a chat lands at the latest message
  const active = activeSession();
  if (active) active.unread = false;
  for (const s of state.sessions) s.turnsEl.hidden = s.id !== id;
  if (active) {
    // Load this chat's per-chat composer settings + draft into the UI.
    const providerChanged = state.provider !== active.provider;
    state.provider = active.provider;
    state.model = active.model;
    state.effort = active.effort;
    state.mode = active.mode;
    state.permission = active.permission;
    state.auto = active.auto;
    document.querySelectorAll("#mode button").forEach((b) =>
      b.classList.toggle("active", (b as HTMLElement).dataset.v === state.mode));
    permChip.hidden = state.mode === "ask";
    input.value = active.draft;
    histIdx = -1; // reset prompt-history recall for the newly-active chat
    input.style.height = "auto";
    input.style.height = Math.min(input.scrollHeight, 220) + "px";
    updateChips();
    if (providerChanged) loadModels();

    // Lazily resume a restored chat's transcript on first open.
    if (active.started && active.threadId && !active.resumed && active.turnsEl.children.length === 0) {
      active.resumed = true;
      const sid = active.id;
      (async () => {
        // read_transcript rebuilds the FULL transcript (incl. thinking and
        // commands, which thread/resume drops) from codex's on-disk rollout —
        // it only reads disk, so it works even before the harness is ready.
        let items: any[] = [];
        try { items = await invoke<any[]>("read_transcript", { threadId: active.threadId }); } catch { /* handled below */ }
        const s = state.sessions.find((x) => x.id === sid);
        if (s && items && items.length) { s.turnsEl.innerHTML = ""; s.blocks.clear(); renderTranscript(s, items); }
        // Record the thread's true creation provider (from the rollout) so a
        // later provider switch starts a new thread — codex binds a thread to
        // its creation provider. Only set if we don't already know it.
        if (s && !s.threadProvider && s.threadId) {
          try {
            const tp = await invoke<string>("thread_provider", { threadId: s.threadId });
            if (tp) s.threadProvider = tp;
          } catch { /* leave unknown */ }
        }
        // resume_thread makes the thread live again so it can be continued;
        // also used as a fallback renderer if the rollout couldn't be read.
        try {
          const thread = await invoke<any>("resume_thread", { threadId: active.threadId });
          const s2 = state.sessions.find((x) => x.id === sid);
          if (s2 && (!items || !items.length)) { s2.turnsEl.innerHTML = ""; s2.blocks.clear(); renderHistory(s2, thread); }
        } catch (e) {
          if (!items || !items.length) addError(active, "Could not resume this chat: " + String(e));
        }
      })();
    } else if (active.turnsEl.children.length === 0) {
      renderEmpty(active);
    }
    setSendButton(active.running);
    drainQueue(active);
  }
  updateHint();
  renderSidebar();
  scrollToBottom();
}

const archList = document.getElementById("archlist") as HTMLDivElement;
const archLabel = document.getElementById("archlabel") as HTMLElement;
const recentsList = document.getElementById("recentslist") as HTMLDivElement;

type Recent = { id: string; preview: string; updatedAt?: number; recencyAt?: number };
let recents: Recent[] = [];
// How many chats the sidebar renders before a "Show more" button; grows by a
// page each time it's clicked so the whole history is reachable.
const CHAT_PAGE = 80;
let chatDisplayLimit = CHAT_PAGE;

async function loadRecents() {
  try {
    // Read straight from the on-disk rollouts — always available, no harness /
    // Docker dependency, and returns the FULL history (paged in the sidebar).
    const data = await invoke<Recent[]>("list_recents");
    recents = Array.isArray(data) ? data : [];
    // Re-render the whole sidebar: the Archive section (persisted archived
    // chats) is derived from `recents`, not just the Recents list.
    renderSidebar();
  } catch {
    // harness not ready / no history yet
  }
}

// Open a chat from the on-disk Recents/Archive lists. `keepArchived` opens an
// archived chat for viewing without moving it back to Recents.
function resumeRecent(r: Recent, keepArchived = false) {
  // Already open? just select it.
  const existing = state.sessions.find((s) => s.threadId === r.id);
  if (existing) { if (!keepArchived) existing.archived = false; selectSession(existing.id); return; }

  const s = makeSession();
  s.threadId = r.id;
  s.title = (r.preview || "Chat").slice(0, 40);
  s.started = true;
  s.archived = keepArchived;
  s.recency = r.recencyAt ?? r.updatedAt ?? Date.now(); // keep its place in the list
  s.draft = loadDraftFor(r.id); // restore any unsent draft for this chat
  state.sessions.push(s);
  // selectSession lazily resumes the transcript and renders it exactly once
  // (clearing the block cache first), then refreshes the sidebar. Doing the
  // resume here as well would double-render and blank the assistant output.
  selectSession(s.id);
}

function itemText(content: any): string {
  if (Array.isArray(content)) {
    return content.filter((c) => c?.type === "text").map((c) => c.text).join(" ");
  }
  return typeof content === "string" ? content : "";
}

function renderHistory(session: Session, thread: any) {
  const turns = thread?.turns ?? [];
  historyRestoring = true; // restored blocks paint instantly, not animated
  try {
    for (const turn of turns) {
      const items = turn?.items ?? [];
      for (const item of items) {
        if (!item?.type) continue;
        if (item.type === "userMessage") {
          const t = itemText(item.content);
          addUserMessage(session, t);
          session.lastText = t; // remember last user prompt for Try again
          session.lastImages = [];
        } else if (item.type === "hookPrompt") {
          // skip
        } else if (item.id) {
          ensureBlock(session, item.id, item.type).setFinal?.(item);
        }
      }
    }
  } finally {
    historyRestoring = false;
  }
  if (session.turnsEl.children.length === 0) renderEmpty(session);
  scrollToBottom();
}

// Render a flat, ordered transcript (from read_transcript / codex rollout):
// userMessage, agentMessage, reasoning and commandExecution items in sequence.
// Cap how many items a restored transcript renders at once. A long conversation
// can have thousands of items; rendering them all (DOM + markdown per item)
// freezes the UI. Render the most recent ones instantly, with a button to
// expand the rest on demand.
const TRANSCRIPT_CAP = 60;
function renderTranscript(session: Session, items: any[], full = false) {
  const start = full ? 0 : Math.max(0, items.length - TRANSCRIPT_CAP);
  session.sentHistory = []; // rebuilt from the transcript below
  historyRestoring = true;
  try {
    if (start > 0) {
      const btn = el("button", "transcript-more", `↑ Show ${start} earlier message${start > 1 ? "s" : ""}`);
      btn.addEventListener("click", () => {
        session.turnsEl.innerHTML = "";
        session.blocks.clear();
        renderTranscript(session, items, true);
        stickToBottom = false;
        scrollEl.scrollTop = 0; // keep the reader near the newly-revealed top
      });
      session.turnsEl.appendChild(btn);
    }
    for (const item of items.slice(start)) {
      if (!item?.type) continue;
      if (item.type === "userMessage") {
        const t = itemText(item.content);
        addUserMessage(session, t);
        session.lastText = t;
        session.lastImages = [];
        if (t && session.sentHistory[session.sentHistory.length - 1] !== t) session.sentHistory.push(t);
      } else if (item.id) {
        ensureBlock(session, item.id, item.type).setFinal?.(item);
      }
    }
    collapseReasoning(session); // fold any trailing reasoning card
  } finally {
    historyRestoring = false;
  }
  if (session.turnsEl.children.length === 0) renderEmpty(session);
  if (!full) scrollToBottom(true);
}

function renderSidebar() {
  // Only list chats that have actually sent a message.
  const active = state.sessions.filter((s) => !s.archived && s.started);
  const archived = state.sessions.filter((s) => s.archived && s.started);

  // ONE unified, recency-sorted Recents list: in-memory open chats + on-disk
  // recents merged and de-duped by threadId. Rendering both in a single list
  // (whole row clickable, stable order) fixes the old two-list behavior where
  // opening a recent made it jump to a separate list and clicks missed the row.
  type Row = { key: string; tid: string | null; title: string; sub: string; recency: number; session?: Session };
  const openByThread = new Map<string, Session>();
  for (const s of active) if (s.threadId) openByThread.set(s.threadId, s);

  const rows: Row[] = [];
  for (const s of active) {
    rows.push({ key: s.id, tid: s.threadId, title: s.title, sub: shortPath(s.workingDir), recency: s.recency, session: s });
  }
  for (const r of recents) {
    if (!r.id || openByThread.has(r.id) || archivedIds.has(r.id) || hiddenIds.has(r.id)) continue;
    rows.push({ key: r.id, tid: r.id, title: (r.preview || "Untitled chat").slice(0, 60), sub: "", recency: r.recencyAt ?? r.updatedAt ?? 0 });
  }
  // Pinned chats float to the top (still recency-sorted within each group).
  const isPinned = (row: Row) => !!row.tid && pinnedIds.has(row.tid);
  rows.sort((a, b) => (Number(isPinned(b)) - Number(isPinned(a))) || (b.recency - a.recency));

  const rowEl = (row: Row): HTMLElement => {
    const s = row.session;
    const pinned = isPinned(row);
    const item = el("div", "item2" + (s && s.id === state.activeSessionId ? " active" : "") + (pinned ? " pinned" : ""));
    if (s) item.dataset.id = s.id;
    if (row.tid) item.dataset.tid = row.tid;
    const statusCls = s?.running ? "running" : s?.unread ? "unread" : "";
    item.innerHTML =
      `<span class="i2-status ${statusCls}"></span>` +
      `<div class="i2-main"><span class="t">${escapeHtml(row.title)}</span>${row.sub ? `<span class="d">${escapeHtml(row.sub)}</span>` : ""}</div>` +
      `<span class="i2-actions">` +
        `<button class="i2-btn pin" title="${pinned ? "Unpin" : "Pin"}">${pinned ? PINNED_ICON : PIN_ICON}</button>` +
        `<button class="i2-btn" title="Archive chat">${ARCHIVE_ICON}</button>` +
      `</span>`;
    if (s) item.title = s.workingDir;
    item.addEventListener("click", () => {
      if (s) selectSession(s.id);
      else { const r = recents.find((x) => x.id === row.key); if (r) resumeRecent(r); }
    });
    item.querySelector(".i2-btn.pin")!.addEventListener("click", (e) => { e.stopPropagation(); togglePin(row.tid); });
    item.querySelectorAll(".i2-btn")[1].addEventListener("click", (e) => {
      e.stopPropagation();
      if (s) archiveSession(s.id);
      else { archivedIds.add(row.key); saveSet("hacksor-archived", archivedIds); renderSidebar(); }
    });
    return item;
  };

  sessList.innerHTML = "";
  recentsList.innerHTML = ""; // unified into sessList now
  const shown = Math.min(rows.length, chatDisplayLimit);
  for (const row of rows.slice(0, shown)) {
    sessList.appendChild(rowEl(row));
  }
  // "Show more" when there are more chats than the current cap — reveal the rest
  // in pages so the full history is reachable without an unbounded initial render.
  if (rows.length > shown) {
    const more = el("button", "chat-more", `Show ${Math.min(rows.length - shown, CHAT_PAGE)} more (${rows.length - shown} older)`);
    more.addEventListener("click", () => { chatDisplayLimit += CHAT_PAGE; renderSidebar(); });
    sessList.appendChild(more);
  }

  archList.innerHTML = "";
  for (const s of [...archived].reverse()) {
    const item = el("div", "item2 arch-item");
    item.dataset.id = s.id;
    item.innerHTML =
      `<div class="i2-main"><span class="t">${escapeHtml(s.title)}</span><span class="d">${escapeHtml(shortPath(s.workingDir))}</span></div>` +
      `<span class="i2-actions">` +
        `<button class="i2-btn restore" title="Restore">${RESTORE_ICON}</button>` +
        `<button class="i2-btn danger" title="Delete permanently">${DELETE_ICON}</button>` +
      `</span>`;
    item.title = s.workingDir;
    // Open for viewing but keep it archived; the Restore button un-archives.
    item.querySelector(".i2-main")!.addEventListener("click", () => selectSession(s.id));
    const btns = item.querySelectorAll(".i2-btn");
    btns[0].addEventListener("click", (e) => { e.stopPropagation(); unarchiveSession(s.id, true); });
    btns[1].addEventListener("click", (e) => { e.stopPropagation(); deleteSession(s.id); });
    archList.appendChild(item);
  }

  // Archived chats persisted from earlier runs (from disk Recents).
  const openIds = new Set(state.sessions.map((s) => s.threadId).filter(Boolean));
  const inMemArchivedThreads = new Set(archived.map((s) => s.threadId).filter(Boolean));
  const diskArchived = recents.filter(
    (r) => archivedIds.has(r.id) && !hiddenIds.has(r.id) && !openIds.has(r.id) && !inMemArchivedThreads.has(r.id),
  );
  for (const r of diskArchived) {
    const item = el("div", "item2 arch-item");
    const label = (r.preview || "Chat").slice(0, 60);
    item.innerHTML =
      `<div class="i2-main"><span class="t">${escapeHtml(label)}</span></div>` +
      `<span class="i2-actions">` +
        `<button class="i2-btn restore" title="Restore">${RESTORE_ICON}</button>` +
        `<button class="i2-btn danger" title="Delete permanently">${DELETE_ICON}</button>` +
      `</span>`;
    item.title = label;
    const restore = () => { archivedIds.delete(r.id); saveSet("hacksor-archived", archivedIds); renderSidebar(); };
    // Open for viewing while staying archived; the Restore button un-archives.
    item.querySelector(".i2-main")!.addEventListener("click", () => resumeRecent(r, true));
    const btns = item.querySelectorAll(".i2-btn");
    btns[0].addEventListener("click", (e) => { e.stopPropagation(); restore(); });
    btns[1].addEventListener("click", (e) => {
      e.stopPropagation();
      hiddenIds.add(r.id); archivedIds.delete(r.id);
      saveSet("hacksor-hidden", hiddenIds); saveSet("hacksor-archived", archivedIds);
      renderSidebar();
    });
    archList.appendChild(item);
  }

  archLabel.hidden = archived.length === 0 && diskArchived.length === 0;

  persistOpenChats();
}

function sessionItemEl(id: string): HTMLElement | null {
  return document.querySelector(`.item2[data-id="${id}"]`);
}

function renameSession(id: string) {
  const s = state.sessions.find((x) => x.id === id);
  const item = sessionItemEl(id);
  const t = item?.querySelector(".t") as HTMLElement | null;
  if (!s || !t) return;
  const input = el("input", "rename-input") as HTMLInputElement;
  input.value = s.title;
  t.replaceWith(input);
  input.focus();
  input.select();
  let done = false;
  const commit = (save: boolean) => {
    if (done) return;
    done = true;
    const val = input.value.trim();
    if (save && val) s.title = val;
    renderSidebar();
  };
  input.addEventListener("keydown", (e) => {
    e.stopPropagation();
    if (e.key === "Enter") { e.preventDefault(); commit(true); }
    else if (e.key === "Escape") { e.preventDefault(); commit(false); }
  });
  input.addEventListener("blur", () => commit(true));
  input.addEventListener("click", (e) => e.stopPropagation());
}

function archiveSession(id: string) {
  const s = state.sessions.find((x) => x.id === id);
  if (!s) return;
  const dom = sessionItemEl(id);
  const finish = () => {
    s.archived = true;
    if (s.threadId) { archivedIds.add(s.threadId); saveSet("hacksor-archived", archivedIds); }
    if (state.activeSessionId === id) {
      const nextActive = state.sessions.filter((x) => !x.archived);
      if (nextActive.length) selectSession(nextActive[nextActive.length - 1].id);
      else newSession();
    } else {
      renderSidebar();
    }
  };
  if (dom) { dom.classList.add("item-exit"); setTimeout(finish, 180); } else finish();
}

function unarchiveSession(id: string, select: boolean) {
  const s = state.sessions.find((x) => x.id === id);
  if (!s) return;
  s.archived = false;
  if (s.threadId) { archivedIds.delete(s.threadId); saveSet("hacksor-archived", archivedIds); }
  if (select) selectSession(id);
  else renderSidebar();
}

function deleteSession(id: string) {
  const s = state.sessions.find((x) => x.id === id);
  if (!s) return;
  const dom = sessionItemEl(id);
  const finish = () => {
    s.turnsEl.remove();
    if (s.threadId) {
      hiddenIds.add(s.threadId); archivedIds.delete(s.threadId);
      saveSet("hacksor-hidden", hiddenIds); saveSet("hacksor-archived", archivedIds);
    }
    state.sessions = state.sessions.filter((x) => x.id !== id);
    if (state.activeSessionId === id) {
      const active = state.sessions.filter((x) => !x.archived);
      if (active.length) selectSession(active[active.length - 1].id);
      else newSession();
    } else {
      renderSidebar();
    }
  };
  if (dom) { dom.classList.add("item-exit"); setTimeout(finish, 180); } else finish();
}

async function pickWorkingDir() {
  const d = await invoke<string | null>("pick_directory");
  if (!d) return;
  // Set the ACTIVE chat's working dir, and remember it as the default for new chats.
  const s = activeSession();
  if (s) s.workingDir = d;
  state.workingDir = d;
  await invoke("save_settings", { args: { working_dir: d } });
  updateHint();
  renderSidebar();
  // Refresh the greeting path if this chat hasn't started yet.
  if (s && s.turnsEl.querySelector(".empty")) {
    s.turnsEl.innerHTML = "";
    renderEmpty(s);
  }
}

// ---------------------------------------------------------------- boot

async function boot() {
  state.providers = await invoke<ProviderInfo[]>("list_providers");
  const s = await invoke<SettingsView>("get_settings");
  state.provider = s.provider;
  state.personality = s.personality;
  state.customInstructions = s.custom_instructions ?? "";
  state.workingDir = s.working_dir;
  state.kaliMode = s.kali_mode;
  state.runtime = s.runtime;
  state.keys = { openrouter: s.has_openrouter_key, vercel: s.has_vercel_key };

  // Restore saved composer preferences.
  const prefs = loadPrefs();
  if (prefs) {
    if (prefs.provider) state.provider = prefs.provider;
    if (prefs.model) state.model = prefs.model;
    state.auto = !!prefs.auto;
    if (prefs.effort) state.effort = prefs.effort;
    if (prefs.mode) state.mode = prefs.mode;
    if (prefs.permission) state.permission = prefs.permission;
  }
  document.querySelectorAll("#mode button").forEach((b) =>
    b.classList.toggle("active", (b as HTMLElement).dataset.v === state.mode));
  permChip.hidden = state.mode === "ask";

  newSession();

  // Restore previously open chats (metadata only; transcript resumes on select).
  for (const c of loadOpenChats()) {
    if (!c.threadId || hiddenIds.has(c.threadId)) continue;
    if (state.sessions.some((s) => s.threadId === c.threadId)) continue;
    const s = makeSession();
    s.threadId = c.threadId;
    s.title = c.title || "Chat";
    s.started = true;
    s.resumed = false;
    s.workingDir = c.workingDir || state.workingDir;
    s.provider = c.provider || state.provider;
    // A restored thread was created under its stored provider; record it so a
    // later provider switch starts a new thread (codex pins provider at creation).
    s.threadProvider = c.provider || undefined;
    s.model = c.model || state.model;
    s.effort = c.effort || state.effort;
    s.mode = c.mode || state.mode;
    s.permission = c.permission || state.permission;
    s.auto = !!c.auto;
    s.role = c.role || "default";
    s.archived = archivedIds.has(c.threadId);
    s.draft = loadDraftFor(c.threadId);
    state.sessions.push(s);
  }

  updateChips();
  updateHint();
  renderSidebar();
  await loadModels();
  loadRecents();
  // First-run onboarding: with no key configured for any provider, open Settings
  // straight away so the user lands on the one thing they must do to begin.
  if (!state.keys.openrouter && !state.keys.vercel) {
    hint.textContent = "Add an API key in Settings to begin.";
    openSettings(true);
  }
  // Provision the runtime UP FRONT (download/build the image + start the
  // container + proxy) so it's ready before the first message — not lazily on
  // send. Runs in the background; progress shows in the runtime banner.
  prepareRuntime();
}

// Kick off runtime provisioning (Docker pull/build/start) without blocking the
// UI. The backend is idempotent, so this is safe to call on startup and after a
// runtime switch. Errors surface in the banner (e.g. Docker not installed).
function prepareRuntime() {
  invoke("prepare_runtime").catch((e) => {
    // Provisioning failed (e.g. Docker missing, or the build errored): never leave
    // the composer stuck as blocked. Unblock it and surface the reason in a
    // persistent banner (a Docker-missing error is returned before any runtime
    // event fires, so we must create the banner here).
    runtimeBusy = false;
    applyRuntimeLock();
    const msg = String(e);
    const banner = ensureRuntimeBanner();
    banner.classList.remove("busy");
    banner.classList.add("error");
    (banner.querySelector(".rt-view") as HTMLElement).hidden = true;
    (banner.querySelector(".rt-bar") as HTMLElement).hidden = true;
    (banner.querySelector(".rt-pct") as HTMLElement).textContent = "";
    const msgEl = banner.querySelector(".rt-msg") as HTMLElement;
    // The backend already distinguishes "not installed" from "installed but the
    // daemon isn't running" (see runtime::docker_unavailable_reason) — surface
    // its own wording rather than collapsing both cases into one message, and
    // only offer the Docker Desktop download link when it's actually missing.
    const notInstalled = /docker/i.test(msg) && /not installed/i.test(msg);
    if (notInstalled) {
      msgEl.innerHTML =
        `⚠ Docker is not installed. Install <a class="rt-link" href="https://www.docker.com/products/docker-desktop/" target="_blank" rel="noreferrer">Docker Desktop</a> (docker.com) to use the runtime, or switch Runtime to Host in Settings.`;
    } else {
      msgEl.textContent = "⚠ " + msg;
    }
    if (/docker/i.test(msg)) hint.textContent = msg;
  });
}

function hasKeyFor(prov: string): boolean {
  if (prov === "opencodex") return true; // local proxy; keys live in its own config
  return prov === "vercel" ? state.keys.vercel : state.keys.openrouter;
}

const COMPLEX_RE = /exploit|reverse|privilege|escalat|payload|shellcode|deobfus|decompile|analy[sz]|audit|chain|bypass|malware|forensic|crack|kernel|binary|fuzz/i;

// Tiny/limited variants Auto avoids for the STRONG tier (kept for the FAST tier).
const TINY_RE = /fable|flash|mini|nano|lite|air|small|instant|haiku|embed|guard|tts|whisper|:free/i;

// GPT families are excluded from Auto entirely (they flag/decline the offensive
// persona too often). Never route Auto to a GPT/OpenAI model.
const AUTO_EXCLUDE = /(^|[\/-])(gpt-|o[1-9]-)|openai\//i;

// Family priority for COMPLEX tasks (strongest first). Provider-agnostic — Auto
// runs over whatever the SELECTED provider offers. Order (per preference):
// DeepSeek V4.1 Flash → Kimi K3 → Grok 4.6 → GLM 5.3.
const STRONG_PREFS: ((id: string) => boolean)[] = [
  (id) => /deepseek/.test(id) && /v4[.\-_]?1/.test(id) && /flash/.test(id),
  (id) => /kimi-k3/.test(id),
  (id) => /grok-?4\.6|grok-?4|grok-code|grok-latest/.test(id),
  (id) => /glm-5\.3/.test(id) && !TINY_RE.test(id),
];
// Family priority for SIMPLE tasks (fast/cheap first), across providers:
// Claude Haiku → GLM flash/turbo → DeepSeek flash → any light (no GPT).
const FAST_PREFS: ((id: string) => boolean)[] = [
  (id) => /claude-haiku|(^|[\/-])haiku/.test(id),
  (id) => /glm.*(flash|turbo)/.test(id),
  (id) => /deepseek.*flash/.test(id),
  (id) => /flash|mini|fast|lite|air|nano/.test(id),
];

// "Auto" router: pick the best model for the task FROM THE SELECTED PROVIDER's
// live catalog (state.models is already provider-scoped). Strong model for
// complex/offensive tasks, fast one for simple. It PREFERS capable families but
// never excludes the provider's models — so Auto works whether you're on
// OpenRouter, Vercel or OpenCodex (any upstream). Effort ladder cheapest → most.
const EFFORT_LADDER = ["low", "medium", "high", "xhigh"];
const ESCALATE_CAP = 3;

// Ordered model candidates for a task (best-first) plus the baseline effort
// index. Always drawn from the selected provider's catalog; guaranteed non-empty.
function autoCandidates(text: string): { models: string[]; baseEffort: number } {
  // Never route Auto to excluded (GPT) families, even as a fallback — unless the
  // provider offers literally nothing else.
  let all = state.models.map((m) => m.id).filter((id) => !AUTO_EXCLUDE.test(id.toLowerCase()));
  if (!all.length) all = state.models.map((m) => m.id);
  if (!all.length) return { models: [], baseEffort: 2 };
  const complex = text.length > 240 || COMPLEX_RE.test(text);
  const prefs = complex ? STRONG_PREFS : FAST_PREFS;
  const ordered: string[] = [];
  const add = (id: string) => { if (!ordered.includes(id)) ordered.push(id); };
  // 1) preferred families, best-first
  for (const p of prefs) for (const id of all) if (p(id.toLowerCase())) add(id);
  // 2) for complex, any remaining capable (non-tiny) model; for simple, anything
  for (const id of all) if (!complex || !TINY_RE.test(id.toLowerCase())) add(id);
  // 3) finally anything left — guarantees a pick from the selected provider
  for (const id of all) add(id);
  return { models: ordered, baseEffort: complex ? 2 : 0 }; // high vs low
}

// Compute-optimal escalation: raise effort first, then step up the model.
function autoPlan(text: string, escalation: number): { model: string; effort: string } {
  const { models, baseEffort } = autoCandidates(text);
  if (!models.length) return { model: state.model, effort: "high" };
  const maxEff = EFFORT_LADDER.length - 1;
  let effortIdx = baseEffort + escalation;
  let modelIdx = 0;
  if (effortIdx > maxEff) {
    modelIdx = effortIdx - maxEff;
    effortIdx = maxEff;
  }
  modelIdx = Math.min(modelIdx, models.length - 1);
  return { model: models[modelIdx], effort: EFFORT_LADDER[effortIdx] };
}

// A cheap/fast model from the selected provider's catalog to gate Smart Auto.
function verifierModel(): string {
  const m = state.models.find((x) => /flash|mini|fast|lite|nano|haiku|air/i.test(x.id));
  return m?.id ?? state.model;
}

// Choose a sensible default model. The old fallback used the alphabetically
// first entry, which for an Anthropic/OpenCodex catalog is `claude-fable-5` — a
// small model that also declines the security persona. Instead: skip tiny/lite
// variants, prefer a strong high-reasoning model, and never silently land on
// fable/haiku.
function pickDefaultModel(models: Model[]): string {
  if (!models.length) return "";
  const tiny = /fable|haiku|flash|mini|nano|lite|small|embed|guard|moderation|tts|whisper|instant/i;
  const strong = /grok|opus|gpt-5|sonnet-5|sonnet-4|kimi|deepseek|llama-3\.[1-9]|qwen|mistral-large|command-r-plus/i;
  const capable = models.filter((m) => !tiny.test(m.id));
  const pool = capable.length ? capable : models;
  return (
    pool.find((m) => strong.test(m.id) && m.reasoning_effort === "high")?.id ??
    pool.find((m) => m.reasoning_effort === "high")?.id ??
    pool[0]?.id ??
    models[0].id
  );
}

async function loadModels() {
  // OpenRouter/Vercel need their key configured in the proxy before their
  // upstream has any models; the OpenCodex selection uses ocx's own upstreams.
  if ((state.provider === "vercel" && !state.keys.vercel) ||
      (state.provider === "openrouter" && !state.keys.openrouter)) {
    state.models = [];
    updateChips();
    return;
  }
  try {
    state.models = await invoke<Model[]>("list_models", { provider: state.provider });
    if (!state.model || !state.models.some((m) => m.id === state.model)) {
      state.model = pickDefaultModel(state.models);
    }
    updateChips();
    updateHint();
  } catch (err) {
    addError(activeSession(), "Could not load models: " + String(err));
  }
}

// ---------------------------------------------------------------- transcript

function renderEmpty(session: Session) {
  if (session.turnsEl.children.length !== 0) return;
  const g =
    session.role === "task" ? "Delegated security sub-task" :
    session.role === "validate" ? "Independent validation" :
    GREETINGS[Math.floor(Math.random() * GREETINGS.length)];
  const sub =
    session.role === "task" ? "Describe one bounded, authorized security task with clear success criteria." :
    session.role === "validate" ? "Paste the vulnerability candidate to reproduce-or-falsify independently." :
    `Working in <code>${escapeHtml(shortPath(session.workingDir))}</code> — tools run here on your machine.`;
  const box = el("div", "empty");
  box.innerHTML = `<h1>${g}</h1><p>${sub}</p>`;
  const chips = el("div", "chips");
  for (const sg of SUGGESTIONS) {
    const c = el("button", "chip", `<span>${sg.icon}</span> ${escapeHtml(sg.label)}`);
    c.addEventListener("click", () => { input.value = sg.text; input.focus(); input.dispatchEvent(new Event("input")); });
    chips.appendChild(c);
  }
  box.appendChild(chips);

  // Working-directory chooser for this chat, shown at the start of the chat.
  const wdRow = el("div", "wd-choose");
  const wdBtn = el("button", "wd-pill", `📁 ${escapeHtml(shortPath(session.workingDir) || "choose folder")}`);
  wdBtn.title = session.workingDir;
  wdBtn.addEventListener("click", pickWorkingDir);
  wdRow.append(el("span", "wd-label", "Working directory"), wdBtn);
  box.appendChild(wdRow);

  session.turnsEl.appendChild(box);
}

async function onSend() {
  // Runtime still provisioning: don't send, show the user why (the build log).
  if (runtimeBusy) { showBuildLog(); return; }
  const text = input.value.trim();
  const session = activeSession();
  if (!text || !session) return;
  // A turn is in flight → queue this message and send it when the turn ends.
  if (session.running || session.starting) {
    session.queue.push(text);
    input.value = ""; session.draft = ""; persistDraft(session); input.style.height = "auto";
    renderQueue(session);
    return;
  }
  if (!hasKeyFor(state.provider)) return openSettings(true);

  // Resolve model + effort. Auto = escalation-aware plan (fresh task → level 0);
  // otherwise the chat's chosen model + effort.
  let model: string, effort: string;
  if (state.auto) {
    session.escalation = 0;
    const plan = autoPlan(text, 0);
    model = plan.model;
    effort = plan.effort;
  } else {
    model = state.model;
    effort = state.effort;
  }
  if (!model) return addError(session, "Pick a model first, or enable Auto.");

  // Capture and clear attachments for this message.
  const atts = state.attachments.slice();
  state.attachments = [];
  renderAttachments();
  const images = atts.filter((a) => a.isImage).map((a) => a.path);
  const files = atts.filter((a) => !a.isImage);
  let fullText = text;
  if (files.length) {
    fullText += "\n\nAttached files (read them from disk as needed):\n" + files.map((f) => f.path).join("\n");
  }
  // Auxiliary vision: if an image is attached but the chosen model can't see,
  // auto-route this turn to a vision-capable model on the same provider.
  if (images.length && !modelSupportsVision(model)) {
    const vm = state.models.find((m) => m.supports_vision);
    if (vm) {
      addAutoNoteText(session, `🖼 Using ${vm.id} for this message (the selected model can't read images).`);
      model = vm.id;
    } else {
      addAutoNoteText(session, "⚠ Image attached but no vision-capable model is available for this provider — it may be ignored.");
    }
  }

  input.value = "";
  session.draft = "";
  persistDraft(session);
  input.style.height = "auto";
  session.turnsEl.querySelector(".empty")?.remove();
  if (session.title === "New chat") {
    session.title = text.slice(0, 40) || "New chat";
    renderSidebar();
  }
  session.lastText = fullText;
  session.lastImages = images;
  // Record in the composer's prompt history (skip consecutive duplicates), and
  // reset the up/down recall cursor so the next ArrowUp starts from the newest.
  if (session.sentHistory[session.sentHistory.length - 1] !== text) session.sentHistory.push(text);
  if (session.sentHistory.length > 200) session.sentHistory.shift();
  histIdx = -1;
  if (!session.started) { session.started = true; renderSidebar(); }
  session.recency = Date.now(); // new activity floats this chat to the top
  stickToBottom = true; // sending always jumps to the latest
  addUserMessage(session, text, atts);
  if (state.auto) addAutoNote(session, model);
  // Instant feedback: flip to running and show the thinking indicator *before*
  // the (possibly multi-second) harness spawn + turn round-trip, so the UI never
  // looks frozen after send.
  setSessionRunning(session, true);
  ensureThinking(session);

  try {
    // All composer providers route through the same OpenCodex proxy backend, so
    // switching among them (OpenRouter ↔ Vercel ↔ OpenCodex) is just a per-turn
    // model change on the same thread — no new thread needed. A new thread with
    // carryover is only required when the underlying Codex backend actually
    // differs (kept for forward-compatibility if a non-proxy provider is added).
    if (session.threadId && session.threadProvider && codexBackend(session.threadProvider) !== codexBackend(state.provider)) {
      const from = providerDisplay(session.threadProvider);
      const to = providerDisplay(state.provider);
      let carried = false;
      try {
        const carry = await invoke<string>("build_carryover", { threadId: session.threadId, maxChars: 12000 });
        if (carry && carry.trim()) {
          fullText = `${carry}\n\n---\nContinue with this next request:\n\n${fullText}`;
          carried = true;
        }
      } catch { /* fall back to a clean thread */ }
      session.threadId = null;
      session.resumed = false;
      addAutoNoteText(session, carried
        ? `↪ Switched provider ${from} → ${to}: started a new thread and carried the conversation over (codex binds a thread to one provider).`
        : `↪ Switched provider ${from} → ${to}: started a new thread (a codex thread is bound to one provider; no prior context to carry).`);
    }
    if (!session.threadId) {
      session.starting = true;
      session.threadId = await invoke<string>("start_chat", {
        args: {
          provider: state.provider,
          model,
          mode: state.mode,
          permission: state.permission,
          working_dir: session.workingDir,
          role: session.role,
        },
      });
      session.threadProvider = state.provider;
      session.starting = false;
    }
    setSessionRunning(session, true);
    await invoke("send_message", {
      args: {
        thread_id: session.threadId,
        text: fullText,
        working_dir: session.workingDir,
        model,
        provider: state.provider,
        effort,
        mode: state.mode,
        role: session.role,
        images,
      },
    });
  } catch (err) {
    session.starting = false;
    setSessionRunning(session, false);
    addError(session, String(err));
  }
}

const queueEl = document.getElementById("queue") as HTMLDivElement;
// Show the active chat's queued messages as cancelable chips above the composer.
function renderQueue(session: Session) {
  if (session.id !== state.activeSessionId) return;
  queueEl.innerHTML = "";
  queueEl.hidden = session.queue.length === 0;
  session.queue.forEach((q, i) => {
    const chip = el("div", "queue-chip");
    chip.title = "Steer message — click to edit before it's sent";
    chip.innerHTML = `<span class="qq-num">↑</span><span class="qq-text">${escapeHtml(q)}</span><button class="qq-x" title="Remove">✕</button>`;
    // Click the chip body → pull it back into the composer to edit; it'll re-queue on send.
    chip.querySelector(".qq-text")!.addEventListener("click", () => {
      session.queue.splice(i, 1);
      input.value = q;
      input.focus();
      input.dispatchEvent(new Event("input"));
      renderQueue(session);
    });
    chip.querySelector(".qq-x")!.addEventListener("click", () => { session.queue.splice(i, 1); renderQueue(session); });
    queueEl.appendChild(chip);
  });
}
// Flush the next steer/queued message. While a turn is running, send it as a
// mid-turn steer (codex accepts a new turn/start and steers); otherwise it's the
// next normal message. Called when a command completes and at turn end/select.
function drainQueue(session: Session) {
  if (session.id !== state.activeSessionId) return;
  renderQueue(session);
  if (session.starting || !session.queue.length) return;
  const next = session.queue.shift()!;
  renderQueue(session);
  if (session.running) {
    steerNow(session, next); // inject into the running turn
  } else {
    input.value = next;
    onSend();
  }
}

// Send a steer message into the currently-running turn (codex accepts mid-turn
// input and adjusts course). Shows it as a user bubble tagged "steer".
async function steerNow(session: Session, text: string) {
  if (!session.threadId) { input.value = text; onSend(); return; }
  session.recency = Date.now();
  addUserMessage(session, text, [], true);
  session.lastText = text;
  const model = state.auto ? autoPlan(text, session.escalation).model : state.model;
  const effort = state.auto ? autoPlan(text, session.escalation).effort : state.effort;
  try {
    await invoke("send_message", {
      args: {
        thread_id: session.threadId, text, working_dir: session.workingDir,
        model, provider: state.provider, effort, mode: state.mode, role: session.role, images: [],
      },
    });
  } catch (e) {
    addError(session, "Steer failed: " + String(e));
  }
}

// Doom-loop guard: if the agent runs the SAME command many times in a row it is
// almost certainly stuck. Warn at 4, and interrupt the turn at 6 (ported idea
// from HackerAI's doom-loop detection).
function checkDoomLoop(session: Session, command: string) {
  const sig = command.trim();
  if (!sig) return;
  if (sig === session.cmdSig) session.cmdRepeat = (session.cmdRepeat ?? 1) + 1;
  else { session.cmdSig = sig; session.cmdRepeat = 1; }
  const n = session.cmdRepeat ?? 1;
  if (n === 4) {
    session.turnsEl.appendChild(el("div", "errbar warn", `⚠ The agent has run the same command ${n} times — it may be stuck in a loop.`));
    scrollToBottom();
  }
  if (n >= 6 && session.threadId) {
    invoke("interrupt", { threadId: session.threadId }).catch(() => {});
    setSessionRunning(session, false);
    session.cmdRepeat = 0;
    session.turnsEl.appendChild(el("div", "errbar", "⛔ Stopped: the agent was repeating the same command (possible loop). Send a new instruction to continue."));
    scrollToBottom();
  }
}

function setSessionRunning(session: Session, r: boolean) {
  session.running = r;
  if (r) { session.unread = false; session.cmdSig = undefined; session.cmdRepeat = 0; }
  if (session.id === state.activeSessionId) setSendButton(r);
  if (!r) drainQueue(session);
  renderSidebar();
}
function setSendButton(r: boolean) {
  sendBtn.textContent = r ? "■" : "↑";
  sendBtn.classList.toggle("stop", r);
  sendBtn.onclick = r ? stopRun : onSend;
}
async function stopRun() {
  const s = activeSession();
  if (s?.threadId) await invoke("interrupt", { threadId: s.threadId }).catch(() => {});
  if (s) setSessionRunning(s, false);
}

function addUserMessage(session: Session, text: string, atts: { name: string; isImage: boolean }[] = [], steer = false) {
  const m = el("div", "msg user");
  const bubble = el("div", "bubble" + (steer ? " steer" : ""), escapeHtml(text));
  bubble.dataset.copy = text;
  if (steer) bubble.prepend(Object.assign(el("span", "steer-tag", "↪ steer"), {}));
  if (atts.length) {
    const row = el("div", "msg-atts");
    row.innerHTML = atts.map((a) => `<span class="att-chip mini">${a.isImage ? "🖼" : "📄"} ${escapeHtml(a.name)}</span>`).join("");
    bubble.appendChild(row);
  }
  m.appendChild(bubble);
  addMsgActions(session, m, bubble, false);
  session.turnsEl.appendChild(m);
  scrollToBottom();
}

function addMsgActions(session: Session, msgEl: HTMLElement, bubble: HTMLElement, isAssistant: boolean) {
  const row = el("div", "msg-actions");
  const mk = (label: string, fn: (btn: HTMLButtonElement) => void) => {
    const b = el("button", "msg-act", label);
    b.onclick = () => fn(b);
    row.appendChild(b);
  };
  mk("⧉ Copy", (b) => copyWithFeedback(bubble.dataset.copy ?? bubble.innerText, b));
  if (isAssistant) {
    mk("↻ Try again", (b) => {
      if (!session.threadId || !session.lastText) { flash(b, "Nothing to retry"); return; }
      if (session.running || session.starting) { flash(b, "Still running…"); return; }
      regenerate(session);
    });
  } else {
    mk("✎ Edit", (b) => {
      if (!session.threadId) { flash(b, "Send first"); return; }
      if (session.running || session.starting) { flash(b, "Still running…"); return; }
      startEditMessage(session, msgEl, bubble);
    });
  }
  mk("⑃ Branch", (b) => {
    if (!session.threadId) { flash(b, "Send first"); return; }
    branchSession(session);
  });
  if (isAssistant) {
    // Tag this answer with the turn's time window + selection so Info can pull
    // the authoritative served model/tokens from the ocx usage log.
    if (session.turnStartMs) msgEl.dataset.tstart = String(session.turnStartMs);
    msgEl.dataset.reqModel = state.model || "";
    msgEl.dataset.reqProvider = state.provider || "";
    msgEl.dataset.reqEffort = state.auto ? "auto" : (state.effort || "");
    session.lastAnswerEl = msgEl;
    mk("ⓘ Info", (b) => openAnswerInfo(msgEl, b));
  }
  msgEl.appendChild(row);
}

let openInfoEl: HTMLElement | null = null;
function closeAnswerInfo() { openInfoEl?.remove(); openInfoEl = null; }

// Show "response details" for one answer: which model actually served it
// (authoritative, from the OpenCodex usage log), provider, status, effort,
// duration and token usage. Correlated to the turn by its time window.
async function openAnswerInfo(msgEl: HTMLElement, btn: HTMLButtonElement) {
  closeAnswerInfo();
  const d = msgEl.dataset;
  const tstart = d.tstart ? Number(d.tstart) : 0;
  const tend = d.tend ? Number(d.tend) : Date.now();
  const fmtN = (n: number) => n.toLocaleString();
  const row = (k: string, v: string, hi = false) => `<div class="info-row${hi ? " hi" : ""}"><span>${k}</span><b>${escapeHtml(v)}</b></div>`;

  let recs: any[] = [];
  try { recs = await invoke<any[]>("usage_recent", { limit: 80 }); } catch { /* proxy log unreadable */ }
  // Records the proxy logged during this turn's window (small margin each side).
  const win = recs.filter((r) => {
    const ts = Number(r.timestamp || 0);
    return tstart && ts >= tstart - 4000 && ts <= tend + 4000;
  });
  const last = win[win.length - 1];
  const sum = (f: string) => win.reduce((a, r) => a + Number(r?.usage?.[f] || 0), 0);

  let body: string;
  if (last) {
    const served = last.resolvedModel || last.model || "?";
    const provider = last.provider || (served.includes("/") ? served.split("/")[0] : "?");
    const status = Number(last.status || 0);
    const durMs = Number(last.durationMs || d.durationMs || 0);
    const inTok = sum("inputTokens"), outTok = sum("outputTokens");
    const totTok = sum("totalTokens") || (inTok + outTok);
    const cached = sum("cacheReadInputTokens") || sum("cachedInputTokens");
    body =
      row("Provider", String(provider), true) +
      row("Model (served)", String(served), true) +
      (last.requestedModel && last.requestedModel !== served ? row("Requested", String(last.requestedModel)) : "") +
      row("Status", status ? `${status}${status === 200 ? " OK" : ""}` : "—") +
      (d.reqEffort ? row("Effort", d.reqEffort) : "") +
      (durMs ? row("Duration", durMs >= 1000 ? `${(durMs / 1000).toFixed(1)}s` : `${durMs} ms`) : "") +
      `<div class="info-sep">Tokens</div>` +
      row("Input", fmtN(inTok)) +
      row("Output", fmtN(outTok)) +
      (cached ? row("Cached", fmtN(cached)) : "") +
      row("Total", fmtN(totTok)) +
      `<div class="info-note">Verified from the OpenCodex proxy log — this is the model that actually answered${win.length > 1 ? `, across ${win.length} API calls this turn` : ""}.</div>`;
  } else {
    // No usage record (answer restored from an earlier session, or log cleared).
    body =
      (d.reqProvider ? row("Provider", providerDisplay(d.reqProvider), true) : "") +
      (d.reqModel ? row("Model (requested)", d.reqModel, true) : "") +
      (d.reqEffort ? row("Effort", d.reqEffort) : "") +
      `<div class="info-note">No proxy usage record for this answer — it was restored from an earlier session, or the OpenCodex log was cleared. The values above are the selection at the time it was sent.</div>`;
  }

  const pop = el("div", "info-pop");
  pop.innerHTML = `<div class="info-title">Response details</div>${body}`;
  document.body.appendChild(pop);
  openInfoEl = pop;
  // Position under the button, clamped to the viewport.
  const r = btn.getBoundingClientRect();
  const pr = pop.getBoundingClientRect();
  let top = r.bottom + 6;
  if (top + pr.height > window.innerHeight - 8) top = Math.max(8, r.top - pr.height - 6);
  let left = Math.min(r.left, window.innerWidth - pr.width - 8);
  pop.style.top = `${top}px`;
  pop.style.left = `${Math.max(8, left)}px`;
  // Dismiss on outside click / Escape / scroll.
  setTimeout(() => {
    const onDoc = (e: MouseEvent) => { if (openInfoEl && !openInfoEl.contains(e.target as Node) && e.target !== btn) { closeAnswerInfo(); cleanup(); } };
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") { closeAnswerInfo(); cleanup(); } };
    const onScroll = () => { closeAnswerInfo(); cleanup(); };
    const cleanup = () => { document.removeEventListener("mousedown", onDoc); document.removeEventListener("keydown", onKey); window.removeEventListener("scroll", onScroll, true); };
    document.addEventListener("mousedown", onDoc);
    document.addEventListener("keydown", onKey);
    window.addEventListener("scroll", onScroll, true);
  }, 0);
}

// Edit a user message in place, then resend from that point (rolls back the
// turns from here to the end so the conversation re-runs with the new prompt).
function startEditMessage(session: Session, msgEl: HTMLElement, bubble: HTMLElement) {
  const original = bubble.dataset.copy ?? bubble.innerText;
  const editor = el("div", "msg-edit");
  const ta = el("textarea", "msg-edit-area") as HTMLTextAreaElement;
  ta.value = original;
  const bar = el("div", "msg-edit-bar");
  const save = el("button", "primary", "Send") as HTMLButtonElement;
  const cancel = el("button", "ghost", "Cancel") as HTMLButtonElement;
  bar.append(cancel, save);
  editor.append(ta, bar);
  bubble.replaceWith(editor);
  ta.focus();
  ta.style.height = "auto"; ta.style.height = Math.min(ta.scrollHeight, 300) + "px";
  const restore = () => editor.replaceWith(bubble);
  cancel.onclick = restore;
  save.onclick = () => {
    const text = ta.value.trim();
    if (!text) { restore(); return; }
    editResend(session, msgEl, text);
  };
  ta.addEventListener("keydown", (e) => {
    if (e.key === "Escape") { e.preventDefault(); restore(); }
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); save.click(); }
  });
}

function editResend(session: Session, msgEl: HTMLElement, text: string) {
  // Count user turns from this message to the end — that's how many turns to
  // roll back on the server before resubmitting the edited prompt.
  const kids = Array.from(session.turnsEl.children);
  const idx = kids.indexOf(msgEl);
  let turns = 0;
  for (let i = idx; i < kids.length; i++) {
    if ((kids[i] as HTMLElement).classList.contains("user")) turns++;
  }
  turns = Math.max(1, turns);
  // Drop everything from the edited message onward from the view.
  for (let i = kids.length - 1; i >= idx; i--) kids[i].remove();
  session.blocks.clear();
  session.lastText = text;
  session.escalation = 0;
  const plan = state.auto ? autoPlan(text, 0) : null;
  addUserMessage(session, text);
  setSessionRunning(session, true);
  invoke("regenerate", {
    args: {
      thread_id: session.threadId, text, working_dir: session.workingDir,
      model: plan?.model ?? state.model, provider: state.provider,
      effort: plan?.effort ?? state.effort, mode: state.mode, role: session.role,
      images: [], turns,
    },
  }).catch((e) => { setSessionRunning(session, false); addError(session, String(e)); });
}

function flash(btn: HTMLButtonElement, text: string) {
  const orig = btn.textContent;
  btn.textContent = text;
  setTimeout(() => { btn.textContent = orig; }, 1200);
}

async function copyWithFeedback(text: string, btn: HTMLButtonElement) {
  let ok = false;
  try {
    await navigator.clipboard.writeText(text);
    ok = true;
  } catch {
    // Fallback for webview contexts where the async clipboard is blocked.
    try {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      ok = document.execCommand("copy");
      ta.remove();
    } catch {}
  }
  flash(btn, ok ? "✓ Copied" : "Copy failed");
}

async function regenerate(session: Session) {
  if (!session.threadId || !session.lastText || session.running || session.starting) return;
  // Remove the last assistant message (and trailing tool cards) from the view.
  const kids = Array.from(session.turnsEl.children);
  for (let i = kids.length - 1; i >= 0; i--) {
    const k = kids[i] as HTMLElement;
    k.remove();
    if (k.classList.contains("msg") && k.classList.contains("assistant")) break;
  }
  session.escalation = 0;
  const plan = state.auto ? autoPlan(session.lastText, 0) : null;
  const model = plan?.model ?? state.model;
  const effort = plan?.effort ?? state.effort;
  setSessionRunning(session, true);
  try {
    await invoke("regenerate", {
      args: {
        thread_id: session.threadId,
        text: session.lastText,
        working_dir: session.workingDir,
        model,
        provider: state.provider,
        effort,
        mode: state.mode,
        role: session.role,
        images: session.lastImages ?? [],
      },
    });
  } catch (e) {
    setSessionRunning(session, false);
    addError(session, String(e));
  }
}

async function branchSession(session: Session) {
  if (!session.threadId) return addError(session, "Send a message before branching.");
  try {
    const newId = await invoke<string>("fork_thread", { threadId: session.threadId });
    const ns = makeSession();
    ns.title = session.title.replace(/ \(branch\)$/, "") + " (branch)";
    ns.workingDir = session.workingDir;
    ns.threadId = newId;
    ns.started = true;
    ns.lastText = session.lastText;
    ns.lastImages = session.lastImages;
    // Clone the current transcript for visual continuity (static snapshot).
    ns.turnsEl.innerHTML = session.turnsEl.innerHTML;
    ns.turnsEl.querySelectorAll(".msg-actions").forEach((r) => r.remove());
    state.sessions.push(ns);
    selectSession(ns.id);
    const dom = sessionItemEl(ns.id);
    if (dom) { dom.classList.add("item-enter"); setTimeout(() => dom.classList.remove("item-enter"), 220); }
  } catch (e) {
    addError(session, "Branch failed: " + String(e));
  }
}
// Show which model Auto is using. Only prints when it actually CHANGES from the
// previous turn (so it reads as a switch, not repeated noise). Memory is kept —
// switching is a per-turn model override on the same thread.
function addAutoNote(session: Session, model: string) {
  if (session.autoModel === model) return; // unchanged — no note
  const first = !session.autoModel;
  const html = first
    ? `✦ Auto selected <b>${escapeHtml(model)}</b>`
    : `✦ Auto switched <span class="am-from">${escapeHtml(session.autoModel!)}</span> → <b>${escapeHtml(model)}</b> <span class="am-keep">· context kept</span>`;
  session.autoModel = model;
  const note = el("div", "auto-note auto-switch");
  note.innerHTML = html;
  session.turnsEl.appendChild(note);
}
function addAutoNoteText(session: Session, text: string) {
  session.turnsEl.appendChild(el("div", "auto-note", escapeHtml(text)));
}

// A turn ended with no output at all — the model declined/flagged the request.
// Name the model that flagged (authoritative served model from the ocx usage
// log, falling back to the requested model) so it's clear WHO returned nothing.
async function noteEmptyResponse(session: Session, _sinceMs: number, requested?: string) {
  // Name the model the turn actually used (what we sent = what declined). Don't
  // correlate against the usage log — it can grab a later/unrelated request.
  const name = requested || session.turnModel || "The model";
  const note = el("div", "auto-note flagged");
  note.innerHTML = `<b>${escapeHtml(name)}</b> returned an empty response — it declined this request. Some models refuse the offensive-security persona and reply with nothing; try a different model (or, in Auto, it will move on).`;
  session.turnsEl.appendChild(note);
  scrollToBottom();
}
function modelSupportsVision(id: string): boolean {
  const m = state.models.find((x) => x.id === id);
  // Unknown models: assume capable (don't reroute) — only reroute when we know
  // the selected model lacks vision.
  return m ? m.supports_vision : true;
}

function addVerdictBadge(session: Session, v: { verdict: string; reason: string }) {
  if (!v || v.verdict === "unknown") return;
  const cls = v.verdict === "ok" ? "ok" : v.verdict === "partial" ? "partial" : "fail";
  const icon = v.verdict === "ok" ? "✓" : v.verdict === "partial" ? "~" : "✗";
  const reason = v.reason ? " · " + escapeHtml(v.reason) : "";
  session.turnsEl.appendChild(el("div", "verdict " + cls, `${icon} ${v.verdict}${reason}`));
  scrollToBottom();
}

// Smart Auto loop: after a turn, a lightweight verifier judges the outcome; on
// fail/partial (and under the cap) escalate effort-then-model and continue the
// SAME thread with the failure reason (no destructive re-run). Auto-only.
async function maybeEscalate(session: Session, answer: string | undefined | null) {
  if (!session.auto || !session.lastText || !answer) return;
  if (session.escalation >= ESCALATE_CAP) return;
  let v: { verdict: string; reason: string };
  try {
    v = await invoke("verify_turn", {
      args: { provider: session.provider, model: verifierModel(), task: session.lastText, answer },
    });
  } catch {
    return;
  }
  addVerdictBadge(session, v);
  if ((v.verdict === "fail" || v.verdict === "partial") && session.escalation < ESCALATE_CAP) {
    session.escalation++;
    await escalateContinue(session, v.reason || v.verdict);
  }
}

async function escalateContinue(session: Session, reason: string) {
  if (!session.threadId) return;
  const plan = autoPlan(session.lastText || "", session.escalation);
  session.autoModel = plan.model; // keep the switch tracker in sync
  const note = el("div", "auto-note escalate");
  note.innerHTML = `↑ Escalating → <b>${escapeHtml(plan.model)}</b> · ${escapeHtml(plan.effort)} <span class="am-keep">· continuing same task, context kept</span>`;
  session.turnsEl.appendChild(note);
  scrollToBottom();
  const nudge =
    `Your previous attempt did not fully accomplish the task (assessment: ${reason}). ` +
    `Continue on the SAME task with more reasoning budget and finish it. Build on the progress already made; ` +
    `do not repeat completed steps. If you were blocked, try a different technique.`;
  setSessionRunning(session, true);
  try {
    await invoke("send_message", {
      args: {
        thread_id: session.threadId,
        text: nudge,
        working_dir: session.workingDir,
        model: plan.model,
        provider: session.provider,
        effort: plan.effort,
        mode: session.mode,
        role: session.role,
        images: [],
      },
    });
  } catch (e) {
    setSessionRunning(session, false);
    addError(session, String(e));
  }
}
function addError(session: Session | undefined, msg: string) {
  (session?.turnsEl ?? turnsHost).appendChild(el("div", "errbar", escapeHtml(msg)));
  scrollToBottom();
}
// Auto-scroll only while the user is parked at the bottom (see stickToBottom,
// set from the scroll listener near scrollEl). If they scroll up to read during
// a live turn, we stop yanking them down; the "↓ Latest" button jumps back.
function scrollToBottom(force = false) {
  if (!force && !stickToBottom) return;
  const target = scrollEl.scrollHeight - scrollEl.clientHeight;
  // Only flag+move when it actually changes position; a no-op assignment fires
  // no scroll event, which would otherwise leave the flag set and swallow the
  // user's next real scroll.
  if (Math.abs(scrollEl.scrollTop - target) > 1) {
    programmaticScroll = true;
    scrollEl.scrollTop = scrollEl.scrollHeight;
  }
  lastScrollTop = scrollEl.scrollTop;
  jumpBtn.hidden = true;
}

// ---------------------------------------------------------------- item blocks

function ensureThinking(session: Session) {
  // Only skip if an *assistant* block already exists — the user's own message
  // (.msg.user) must not suppress the indicator.
  if (session.thinkingEl || session.turnsEl.querySelector(".msg:not(.user),.card,.plan")) return;
  const e = el("div", "thinking-live", "Thinking");
  session.turnsEl.appendChild(e);
  session.thinkingEl = e;
  // A live elapsed counter turns the pre-token wait into motion + information
  // instead of a frozen spinner.
  const t0 = Date.now();
  session.thinkTimer = window.setInterval(() => {
    const s = Math.round((Date.now() - t0) / 1000);
    e.textContent = s >= 1 ? `Thinking… ${s}s` : "Thinking";
  }, 500);
  scrollToBottom();
}
function removeThinking(session: Session) {
  if (session.thinkTimer) { clearInterval(session.thinkTimer); session.thinkTimer = undefined; }
  if (session.thinkingEl) { session.thinkingEl.remove(); session.thinkingEl = undefined; }
}

// Fold any still-expanded live reasoning card into a "Thought for Ns" summary.
function collapseReasoning(session: Session) {
  session.turnsEl.querySelectorAll(".card.reasoning:not(.collapsed)").forEach((c) => {
    c.classList.add("collapsed");
    const t0 = Number((c as HTMLElement).dataset.t0 || 0);
    const lbl = c.querySelector(".label") as HTMLElement | null;
    // Restored history has no real duration, so just label it "Thoughts".
    if (lbl) lbl.textContent = historyRestoring ? "Thoughts" : (t0 ? `Thought for ${Math.max(1, Math.round((Date.now() - t0) / 1000))}s` : "Thoughts");
  });
}

function ensureBlock(session: Session, id: string, kind: string): ItemBlock {
  removeThinking(session);
  let b = session.blocks.get(id);
  if (b) return b;
  b = createBlock(kind);
  session.blocks.set(id, b);
  session.turnsEl.appendChild(b.root);
  if (kind === "agentMessage") {
    // The model has switched from thinking to answering: fold any live reasoning
    // card into a "Thought for Ns" summary (click the head to re-expand).
    collapseReasoning(session);
    const bubble = b.root.querySelector(".bubble") as HTMLElement;
    if (bubble) addMsgActions(session, b.root, bubble, true);
  }
  scrollToBottom();
  return b;
}

function toolCard(icon: string, label: string, extraCls = "") {
  const card = el("div", `card ${extraCls}`.trim());
  const head = el("div", "head",
    `<span class="icon">${icon}</span><span class="label">${escapeHtml(label)}</span><span class="status"></span><button class="card-expand" title="Open full output">⤢</button>`);
  const body = el("div", "body");
  const pre = el("pre");
  body.appendChild(pre);
  card.append(head, body);
  const labelEl = head.querySelector(".label") as HTMLElement;
  head.addEventListener("click", (e) => {
    if ((e.target as HTMLElement).closest(".card-expand")) return;
    card.classList.toggle("collapsed");
  });
  head.querySelector(".card-expand")!.addEventListener("click", (e) => {
    e.stopPropagation();
    openToolDrawer(icon, labelEl.textContent || label, pre.textContent || "");
  });
  return { card, pre, label: labelEl, status: head.querySelector(".status") as HTMLElement };
}

// Right-side drawer showing one tool's full output, larger and copyable.
function openToolDrawer(icon: string, title: string, content: string) {
  buildDrawerOpen = false; // a tool output, not the live build log (the caller re-sets it)
  const drawer = document.getElementById("tooldrawer") as HTMLElement;
  (document.getElementById("td-icon") as HTMLElement).textContent = icon;
  (document.getElementById("td-title") as HTMLElement).textContent = title;
  (document.getElementById("td-body") as HTMLElement).textContent = content;
  drawer.hidden = false;
  document.getElementById("app")!.classList.add("drawer-open");
}
function closeToolDrawer() {
  buildDrawerOpen = false;
  (document.getElementById("tooldrawer") as HTMLElement).hidden = true;
  document.getElementById("app")!.classList.remove("drawer-open");
}

// ChatGPT-style smooth reveal: incoming deltas (which arrive in bursts, coupled
// to network/provider cadence) fill `received`; a requestAnimationFrame pump
// drains a proportional slice each frame and repaints at most once per frame.
// This decouples display from arrival so bursts read as fluid typing, and it
// replaces the per-delta full-markdown re-render that caused flicker and jank.
// True only while renderHistory replays a restored transcript, so those blocks
// paint instantly instead of animating dozens of old messages.
let historyRestoring = false;

function makeSmoothReveal(paint: (text: string) => void, scroll = true, onDone?: () => void) {
  let received = "";
  let shown = 0;
  let raf = 0;
  const frame = () => {
    raf = 0;
    const backlog = received.length - shown;
    if (backlog > 0) {
      // GPT-style CONSTANT-RATE typing: reveal a small slice each frame so long
      // replies take proportionally longer (never pop). ~2 chars/frame baseline
      // (~120 chars/s) eases at the tail; when we've fallen behind a fast stream
      // or a big buffered reply, accelerate (backlog/8) up to a 22 char/frame cap
      // (~1300 chars/s) so it stays snappy without dumping everything at once.
      const step = Math.min(backlog, Math.max(2, Math.min(22, Math.ceil(backlog / 8))));
      shown = Math.min(received.length, shown + step);
      paint(received.slice(0, shown));
      if (scroll) scrollToBottom();
    }
    if (shown < received.length) raf = requestAnimationFrame(frame);
    else onDone?.();
  };
  const kick = () => { if (!raf) raf = requestAnimationFrame(frame); };
  const snap = () => {
    shown = received.length;
    if (raf) { cancelAnimationFrame(raf); raf = 0; }
    paint(received);
    if (scroll) scrollToBottom();
    onDone?.();
  };
  return {
    push(s: string) { received += s; kick(); return received; },
    // Finalize. History restore paints instantly. Every LIVE reply — whether it
    // streamed deltas or arrived as one buffered chunk — is animated to the end
    // by the pump (kick), so it never snaps/pops the remaining text.
    finish(full: string) {
      received = full;
      if (historyRestoring) snap();
      else kick();
    },
  };
}

function createBlock(kind: string): ItemBlock {
  if (kind === "agentMessage") {
    const m = el("div", "msg assistant");
    const bubble = el("div", "bubble");
    m.appendChild(bubble);
    const reveal = makeSmoothReveal((t) => { bubble.innerHTML = renderMd(t); }, true, () => { enhanceMermaid(bubble); enhanceCodeBlocks(bubble); });
    const b: ItemBlock = {
      root: m, buffer: "",
      appendDelta: (s) => { b.buffer = reveal.push(s); bubble.dataset.copy = b.buffer; },
      setFinal: (item) => {
        const full = item.text ?? b.buffer;
        b.buffer = full;
        bubble.dataset.copy = full;
        reveal.finish(full);
      },
    };
    return b;
  }
  if (kind === "reasoning") {
    // Start EXPANDED so the thinking phase streams live (no dead time); it is
    // auto-collapsed to a "Thought for Ns" summary the moment the final answer
    // begins (see ensureBlock).
    const { card, pre } = toolCard("✳", "Thinking", "reasoning");
    card.dataset.t0 = String(Date.now());
    const reveal = makeSmoothReveal((t) => { pre.textContent = t; });
    const b: ItemBlock = {
      root: card, buffer: "",
      appendDelta: (s) => { b.buffer = reveal.push(s); },
      setFinal: (item) => {
        const full = (item.summary?.join("\n\n") || item.content?.join("\n\n") || b.buffer) ?? "";
        b.buffer = full;
        reveal.finish(full);
      },
    };
    return b;
  }
  if (kind === "commandExecution") {
    const { card, pre, label, status } = toolCard("❯", "command");
    status.textContent = "running…"; status.className = "status running";
    // Click the (truncated) command label to reveal it in full; hover shows a tooltip.
    label.classList.add("cmd-label");
    label.addEventListener("click", (e) => { e.stopPropagation(); label.classList.toggle("expanded"); });
    const b: ItemBlock = {
      root: card, buffer: "",
      appendDelta: (s) => { b.buffer += s; pre.textContent = b.buffer; scrollToBottom(); },
      setFinal: (item) => {
        if (item.command) { label.textContent = item.command; label.title = item.command; }
        if (item.aggregatedOutput) pre.textContent = item.aggregatedOutput;
        const code = item.exitCode, st = item.status;
        if (st === "failed" || (typeof code === "number" && code !== 0)) { status.textContent = `exit ${code ?? "?"}`; status.className = "status fail"; }
        else if (st === "declined") { status.textContent = "declined"; status.className = "status fail"; }
        else if (st === "inProgress") { status.textContent = "running…"; status.className = "status running"; }
        else { status.textContent = `exit ${code ?? 0}`; status.className = "status ok"; }
      },
    };
    return b;
  }
  if (kind === "fileChange") {
    const { card, pre, label, status } = toolCard("✎", "file change", "diff");
    return {
      root: card, buffer: "",
      setFinal: (item) => {
        const changes = item.changes ?? [];
        label.textContent = changes.map((c: any) => c.path).join(", ") || "file change";
        pre.innerHTML = changes.map((c: any) => renderDiff(c.diff ?? "")).join("\n");
        status.textContent = item.status ?? "";
        status.className = "status" + (item.status === "failed" ? " fail" : " ok");
      },
    };
  }
  if (kind === "webSearch") {
    const { card, pre, label } = toolCard("⌕", "web search", "collapsed");
    return {
      root: card, buffer: "",
      setFinal: (item) => { if (item.query) label.textContent = "search: " + item.query; pre.textContent = JSON.stringify(item.results ?? item, null, 2); },
    };
  }
  const { card, pre } = toolCard("•", kind, "collapsed");
  return { root: card, buffer: "", setFinal: (item) => (pre.textContent = JSON.stringify(item, null, 2)) };
}

function renderPlan(session: Session, params: any) {
  const steps = params.plan ?? [];
  if (!session.planEl || session.planEl.parentElement !== session.turnsEl) {
    session.planEl = el("ul", "plan");
    session.turnsEl.appendChild(session.planEl);
  }
  session.planEl.innerHTML = steps.map((s: any) => {
    const status = s.status ?? "pending";
    const mark = status === "completed" ? "✓" : status === "in_progress" ? "▸" : "○";
    return `<li class="${status}"><span class="mark">${mark}</span><span class="txt">${escapeHtml(s.step ?? "")}</span></li>`;
  }).join("");
  scrollToBottom();
}

function renderApproval(session: Session, params: any, kind: "exec" | "patch") {
  const box = el("div", "approval");
  const token = params.token;
  const cmd = kind === "exec"
    ? (Array.isArray(params.command) ? params.command.join(" ") : params.command ?? "")
    : "(file changes)";
  if (kind === "exec") {
    box.appendChild(el("div", "q", `Run this command in <code>${escapeHtml(params.cwd ?? "")}</code>?${params.reason ? "<br>" + escapeHtml(params.reason) : ""}`));
    box.appendChild(el("pre", undefined, escapeHtml(cmd)));
  } else {
    box.appendChild(el("div", "q", `Apply file changes?${params.reason ? "<br>" + escapeHtml(params.reason) : ""}`));
  }
  const done = (approve: boolean) => {
    invoke("submit_approval", { args: { token, approve } }).catch((e) => addError(session, String(e)));
    box.remove();
  };

  // Auto-review mode: a reviewer model decides approve/deny; only genuinely
  // ambiguous actions fall through to the human buttons below.
  if (session.permission === "auto_review") {
    const verdict = el("div", "review-verdict", "⚖ Auto-reviewing…");
    box.appendChild(verdict);
    session.turnsEl.appendChild(box);
    scrollToBottom();
    invoke<{ decision: string; risk: string; reason: string }>("review_action", {
      args: { provider: state.provider, model: reviewerModel(), command: cmd, context: params.reason ?? "" },
    }).then((d) => {
      if (d.decision === "approve") { verdict.textContent = `✓ Auto-approved${d.reason ? " — " + d.reason : ""}`; setTimeout(() => box.remove(), 700); done(true); }
      else if (d.decision === "deny") { verdict.className = "review-verdict deny"; verdict.textContent = `⛔ Auto-denied${d.reason ? " — " + d.reason : ""}`; done(false); }
      else { verdict.textContent = `⚖ Needs your call${d.reason ? " — " + d.reason : ""}`; addApprovalButtons(box, done); }
    }).catch(() => { verdict.textContent = "⚖ Review unavailable — your call:"; addApprovalButtons(box, done); });
    return;
  }

  addApprovalButtons(box, done);
  session.turnsEl.appendChild(box);
  scrollToBottom();
}

function addApprovalButtons(box: HTMLElement, done: (approve: boolean) => void) {
  const row = el("div", "row");
  const yes = el("button", "primary", "Approve");
  const no = el("button", "ghost", "Deny");
  yes.onclick = () => done(true);
  no.onclick = () => done(false);
  row.append(yes, no);
  box.appendChild(row);
}

// Model the reviewer uses: prefer a fast one if present, else the chat's model.
function reviewerModel(): string {
  const fast = state.models.find((m) => /flash|mini|haiku|fast|lite/i.test(m.id));
  return fast?.id ?? state.model;
}

// ---------------------------------------------------------------- events

// Runtime provisioning banner (Docker mode auto-pull/build/start progress).
// Accumulate the Docker build output; update the drawer live if it's open.
listen<{ line: string }>("hacksor://runtime-log", (evt) => {
  runtimeLog += evt.payload.line + "\n";
  if (buildDrawerOpen) {
    const body = document.getElementById("td-body") as HTMLElement;
    body.textContent = runtimeLog;
    body.scrollTop = body.scrollHeight;
  }
});

listen<{ phase: string; message: string; percent?: number }>("hacksor://runtime", (evt) => {
  const { phase, message, percent } = evt.payload;
  const banner = ensureRuntimeBanner();
  banner.classList.remove("error");
  (banner.querySelector(".rt-bar") as HTMLElement).hidden = false;
  const busy = phase === "building" || phase === "starting" || phase === "pulling";
  runtimeBusy = busy;
  applyRuntimeLock();
  const icon = busy ? "⏳ " : "✓ ";
  (banner.querySelector(".rt-msg") as HTMLElement).textContent = icon + message;
  banner.classList.toggle("busy", busy);
  // The "View build output" button is useful once there's build output to show.
  (banner.querySelector(".rt-view") as HTMLElement).hidden = !(phase === "building" || runtimeLog.length > 0);
  // Show a real progress bar while pulling (percent present); hide it otherwise.
  const hasPct = typeof percent === "number" && phase === "pulling";
  const bar = banner.querySelector(".rt-bar") as HTMLElement;
  const fill = banner.querySelector(".rt-fill") as HTMLElement;
  const pctEl = banner.querySelector(".rt-pct") as HTMLElement;
  bar.hidden = !hasPct;
  if (hasPct) {
    fill.style.width = `${Math.max(0, Math.min(100, percent!))}%`;
    pctEl.textContent = `${percent}%`;
  } else {
    pctEl.textContent = "";
  }
  if (phase === "ready") {
    setTimeout(() => { rtBanner?.remove(); rtBanner = null; }, 1800);
  }
});

listen<Notif>("hacksor://event", (evt) => {
  const { method, params } = evt.payload;
  const session = sessionByThread(params?.threadId);
  if (!session) return;
  switch (method) {
    case "turn/started":
      setSessionRunning(session, true);
      session.sawOutputThisTurn = false;
      session.turnStartMs = Date.now();
      // The model this turn actually used: in Auto it's the auto-picked model
      // (tracked in autoModel), not the composer's state.model.
      session.turnModel = (state.auto ? session.autoModel : state.model) || state.model || undefined;
      session.lastAnswerEl = undefined;
      ensureThinking(session);
      break;
    case "turn/completed":
      // Tag the turn's answer with its time window + duration so the Info
      // popover can correlate it to the ocx usage record(s).
      if (session.lastAnswerEl) {
        session.lastAnswerEl.dataset.tend = String(Date.now());
        const dur = params.turn?.durationMs;
        if (typeof dur === "number") session.lastAnswerEl.dataset.durationMs = String(dur);
      }
      removeThinking(session);
      collapseReasoning(session);
      if (session.id !== state.activeSessionId) session.unread = true;
      setSessionRunning(session, false);
      // Empty completion: the turn ended with no assistant text and no error.
      // Most often the model declined the security persona (Anthropic Claude via
      // OpenCodex returns nothing). Surface it — naming the model that flagged —
      // so the user isn't left staring at silence with no explanation.
      if (session.sawOutputThisTurn === false && !(params.lastAgentMessage || "").trim()) {
        noteEmptyResponse(session, session.turnStartMs || Date.now(), session.turnModel);
      }
      maybeEscalate(session, params.lastAgentMessage);
      break;
    case "turn/aborted":
      removeThinking(session);
      collapseReasoning(session);
      if (session.id !== state.activeSessionId) session.unread = true;
      setSessionRunning(session, false);
      break;
    case "error": {
      // Retryable stream errors repeat with willRetry:true; ignore those and
      // only surface the terminal failure.
      if (params.willRetry === true) break;
      removeThinking(session);
      setSessionRunning(session, false);
      const msg = params.error?.message ?? params.message ?? "Unknown error";
      addError(session, msg);
      break;
    }
    case "item/agentMessage/delta": session.sawOutputThisTurn = true; ensureBlock(session, params.itemId, "agentMessage").appendDelta?.(params.delta); break;
    case "item/reasoning/summaryTextDelta":
    case "item/reasoning/textDelta": session.sawOutputThisTurn = true; ensureBlock(session, params.itemId, "reasoning").appendDelta?.(params.delta); break;
    case "item/commandExecution/outputDelta": session.sawOutputThisTurn = true; ensureBlock(session, params.itemId, "commandExecution").appendDelta?.(params.delta); break;
    case "item/started":
    case "item/completed": {
      const item = params.item;
      // User messages are rendered locally on send; skip the echoed item.
      if (!item?.id || !item?.type || item.type === "userMessage" || item.type === "hookPrompt") break;
      // Any rendered item (assistant text, reasoning, a command) counts as a
      // non-empty turn — even a buffered, non-streamed reply.
      session.sawOutputThisTurn = true;
      ensureBlock(session, item.id, item.type).setFinal?.(item);
      if (method === "item/completed" && item.type === "commandExecution") {
        checkDoomLoop(session, item.command || "");
        // "When a command is executed and there's a steer message, steer it."
        if (session.running && session.queue.length) drainQueue(session);
      }
      break;
    }
    case "turn/plan/updated": renderPlan(session, params); break;
    case "approval/exec": renderApproval(session, params, "exec"); break;
    case "approval/patch": renderApproval(session, params, "patch"); break;
    default: break;
  }
});

// ---------------------------------------------------------------- settings

// ---------------------------------------------------------------- notes

type NoteFile = { name: string; content: string };

async function openNotes() {
  const bg = el("div", "modal-bg");
  bg.innerHTML = `
    <div class="modal wide">
      <button class="modal-close" id="n-close" title="Close (Esc)">✕</button>
      <h2>Findings notes</h2>
      <p>Shared with the agent (stored in codex-home/notes). Record findings and methodology here; they persist across chats.</p>
      <div id="n-list" class="notes-list"></div>
      <div class="actions">
        <button id="n-new" class="ghost">＋ New note</button>
      </div>
    </div>`;
  app.appendChild(bg);
  const list = bg.querySelector("#n-list") as HTMLDivElement;
  const close = () => { document.removeEventListener("keydown", esc); bg.remove(); };
  const esc = (e: KeyboardEvent) => { if (e.key === "Escape") { e.preventDefault(); close(); } };
  document.addEventListener("keydown", esc);
  bg.addEventListener("mousedown", (e) => { if (e.target === bg) close(); });
  bg.querySelector("#n-close")!.addEventListener("click", close);

  const render = (notes: NoteFile[]) => {
    list.innerHTML = "";
    if (!notes.length) list.appendChild(el("div", "note-empty", "No notes yet."));
    for (const n of notes) {
      const card = el("div", "note-card");
      card.innerHTML = `<input class="note-name" value="${escapeHtml(n.name.replace(/\.md$/, ""))}" /><textarea class="note-body" rows="4">${escapeHtml(n.content)}</textarea><div class="note-actions"><button class="ghost note-save">Save</button><button class="ghost note-del">Delete</button></div>`;
      const nameI = card.querySelector(".note-name") as HTMLInputElement;
      const body = card.querySelector(".note-body") as HTMLTextAreaElement;
      card.querySelector(".note-save")!.addEventListener("click", async () => {
        await invoke("write_note", { name: nameI.value, content: body.value }).catch(() => {});
        if (n.name && n.name !== (nameI.value.endsWith(".md") ? nameI.value : nameI.value + ".md")) {
          await invoke("delete_note", { name: n.name }).catch(() => {});
        }
        load();
      });
      card.querySelector(".note-del")!.addEventListener("click", async () => {
        if (n.name) await invoke("delete_note", { name: n.name }).catch(() => {});
        load();
      });
      list.appendChild(card);
    }
  };
  const load = async () => {
    const notes = await invoke<NoteFile[]>("read_notes").catch(() => [] as NoteFile[]);
    render(notes);
  };
  bg.querySelector("#n-new")!.addEventListener("click", () => {
    render([{ name: "", content: "" }]);
  });
  load();
}

// ---------------------------------------------------------------- search

function openSearch() {
  const bg = el("div", "modal-bg");
  bg.innerHTML = `
    <div class="modal wide">
      <button class="modal-close" id="q-close" title="Close (Esc)">✕</button>
      <h2>Search chats</h2>
      <input id="q-input" class="search-input" placeholder="Search all chats… (Ctrl/⌘+K)" />
      <div id="q-results" class="search-results"></div>
    </div>`;
  app.appendChild(bg);
  const input2 = bg.querySelector("#q-input") as HTMLInputElement;
  const results = bg.querySelector("#q-results") as HTMLDivElement;
  const close = () => { document.removeEventListener("keydown", esc); bg.remove(); };
  const esc = (e: KeyboardEvent) => { if (e.key === "Escape") { e.preventDefault(); close(); } };
  document.addEventListener("keydown", esc);
  bg.addEventListener("mousedown", (e) => { if (e.target === bg) close(); });
  bg.querySelector("#q-close")!.addEventListener("click", close);
  const openHit = (threadId: string) => {
    const existing = state.sessions.find((s) => s.threadId === threadId);
    if (existing) selectSession(existing.id);
    else {
      const r = recents.find((x) => x.id === threadId) ?? { id: threadId, preview: "" } as Recent;
      resumeRecent(r);
    }
    close();
  };
  let seq = 0;
  const run = async () => {
    const q = input2.value.trim();
    const mine = ++seq;
    results.innerHTML = "";
    if (!q) return;
    // Full-text search across every on-disk chat (rollout files), backend-side.
    let hits: { thread_id: string; title: string; snippet: string }[] = [];
    try { hits = await invoke("search_chats", { query: q }); } catch { /* fall back to loaded */ }
    if (mine !== seq) return; // a newer keystroke superseded this one
    results.innerHTML = "";
    for (const h of hits) {
      const row = el("div", "search-row");
      row.innerHTML = `<div class="sr-title">${escapeHtml(h.title)}</div><div class="sr-snip">${escapeHtml(h.snippet)}</div>`;
      row.addEventListener("click", () => openHit(h.thread_id));
      results.appendChild(row);
    }
    if (!results.children.length) results.appendChild(el("div", "note-empty", "No matches."));
  };
  // Debounce: each keystroke scans every chat on disk, so only search once
  // typing pauses (and skip 1-char queries that match almost everything).
  let debounce: number | undefined;
  input2.addEventListener("input", () => {
    if (debounce) clearTimeout(debounce);
    const q = input2.value.trim();
    if (q.length < 2) { seq++; results.innerHTML = ""; return; } // invalidate in-flight
    debounce = window.setTimeout(run, 180);
  });
  setTimeout(() => input2.focus(), 0);
}

type ServicesStatus = { docker: boolean; proxy_running: boolean; proxy_port: number; browser_running: boolean; browser_port: number; ca_path: string; mode: string };

// Embedded browser: open a full Chromium webview window, or pull page text.
async function exportChat(session: Session) {
  let md = `# ${session.title}\n\n`;
  for (const child of Array.from(session.turnsEl.children) as HTMLElement[]) {
    if (!child.classList.contains("msg")) continue;
    const bubble = child.querySelector(".bubble") as HTMLElement | null;
    if (!bubble) continue;
    const who = child.classList.contains("user") ? "**You:**" : "**Hacksor:**";
    md += `${who}\n\n${bubble.dataset.copy ?? bubble.innerText}\n\n---\n\n`;
  }
  const name = (session.title.replace(/[^\w .-]/g, "_") || "chat") + ".md";
  await invoke("save_text_dialog", { defaultName: name, content: md }).catch((e) => addError(session, String(e)));
}

// Turn a <textarea> into a syntax-highlighted code editor: a highlighted <pre>
// is rendered behind a transparent textarea (caret stays visible, text aligns).
// Returns a refresh() to re-highlight after programmatic value changes.
function attachHighlight(ta: HTMLTextAreaElement, lang: string): () => void {
  const wrap = el("div", "code-edit");
  ta.parentNode!.insertBefore(wrap, ta);
  const pre = el("pre", "code-edit-hl");
  const code = document.createElement("code");
  pre.appendChild(code);
  wrap.appendChild(pre);
  wrap.appendChild(ta);
  ta.classList.add("code-edit-ta");
  const refresh = () => {
    let html: string;
    try { html = hljs.highlight(ta.value, { language: lang }).value; }
    catch { html = escapeHtml(ta.value); }
    code.innerHTML = html + "\n"; // trailing newline keeps heights aligned
  };
  const sync = () => { pre.scrollTop = ta.scrollTop; pre.scrollLeft = ta.scrollLeft; };
  ta.addEventListener("input", refresh);
  ta.addEventListener("scroll", sync);
  refresh();
  return refresh;
}

function openSettings(force = false) {
  const bg = el("div", "modal-bg");
  const personalities = ["", "cynic", "robot", "nerd", "mentor"];
  bg.innerHTML = `
    <div class="modal">
      <button class="modal-close" id="s-close" title="Close (Esc)">✕</button>
      <h2>Settings</h2>
      <div class="tab-bar" id="s-tabs">
        <button class="tab-btn active" data-tab="general">General</button>
        <button class="tab-btn" data-tab="providers">Providers</button>
        <button class="tab-btn" data-tab="agent">Agent</button>
        <button class="tab-btn" data-tab="advanced">Advanced</button>
      </div>

      <div class="tab-pane active" data-pane="general">
        <div class="field">
          <label>Runtime <span id="s-rt-status" class="unset-badge"></span></label>
          <select id="s-runtime">
            <option value="host" ${state.runtime !== "docker" ? "selected" : ""}>Host (requires codex installed)</option>
            <option value="docker" ${state.runtime === "docker" ? "selected" : ""}>Docker (bundled — full toolset; needs only Docker)</option>
          </select>
          <p style="margin:6px 0 0">Docker mode bundles the whole hacking environment (nmap, nuclei, sqlmap, mitmproxy, wordlists…). Everything — image build, container, proxy, and vuln-intel updates — happens automatically on your first message; it's torn down when you quit.</p>
        </div>
        <div class="field">
          <label>Working directory (where tools run)</label>
          <div class="row">
            <input id="s-dir" value="${escapeHtml(state.workingDir)}" />
            <button id="s-pick" class="ghost">Browse</button>
          </div>
        </div>
        <div class="field">
          <label>Theme</label>
          <select id="s-theme">
            <option value="dark" ${document.documentElement.classList.contains("dark") ? "selected" : ""}>Dark</option>
            <option value="light" ${!document.documentElement.classList.contains("dark") ? "selected" : ""}>Light</option>
          </select>
        </div>
        <div class="field">
          <label>Runtime services</label>
          <select id="s-services-mode">
            <option value="on_demand">On demand — start when first used (less RAM)</option>
            <option value="always_on">Always on — pre-start with the container (no warm-up)</option>
          </select>
          <div id="s-services" class="svc-list" style="margin-top:8px"><span class="unset-badge">Checking…</span></div>
          <p style="margin:6px 0 0">The intercepting proxy and Camoufox stealth browser. <b>On demand</b> keeps the heavy browser (Firefox) out of RAM until the agent actually browses; the proxy starts on first capture. <b>Always on</b> trades ~300–800&nbsp;MB of idle RAM for zero first-use latency.</p>
        </div>
      </div>

      <div class="tab-pane" data-pane="providers">
        <p style="margin:0 0 12px;opacity:.75">Every provider runs through the local OpenCodex proxy. The composer's <b>OpenRouter</b> and <b>Vercel</b> selections are just filtered views over the upstreams you configure here — set their keys below, or add any of 40+ other upstreams via OpenCodex. Saving a key restarts the proxy so it takes effect.</p>
        <div class="field">
          <label>OpenRouter API key ${keyBadge(state.keys.openrouter)}</label>
          <div class="key-row">
            <input id="s-or" class="${state.keys.openrouter ? "saved" : ""}" type="password" placeholder="${state.keys.openrouter ? "•••••••••••••• — leave blank to keep" : "sk-or-..."}" />
            ${state.keys.openrouter ? '<button class="key-clear" id="s-or-clear" title="Remove key">Remove</button>' : ""}
          </div>
        </div>
        <div class="field">
          <label>Vercel AI Gateway API key ${keyBadge(state.keys.vercel)}</label>
          <div class="key-row">
            <input id="s-vc" class="${state.keys.vercel ? "saved" : ""}" type="password" placeholder="${state.keys.vercel ? "•••••••••••••• — leave blank to keep" : "vck_..."}" />
            ${state.keys.vercel ? '<button class="key-clear" id="s-vc-clear" title="Remove key">Remove</button>' : ""}
          </div>
        </div>
        <div class="field">
          <label>Other providers via OpenCodex <span id="s-ocx-status" class="unset-badge"></span></label>
          <p style="margin:0 0 10px">One wrapper for 40+ upstreams (Anthropic, OpenAI, Gemini, Groq, DeepSeek, Together, local Ollama…). Add a key, then pick <b>OpenCodex (any provider)</b> in the composer. Subscription/OAuth logins (ChatGPT / Claude Pro) use the dashboard.</p>
          <div id="s-prov-list" class="prov-list"></div>
          <div class="row" style="margin-top:10px">
            <button id="s-ocx-setup" class="ghost">Open provider dashboard (OAuth / advanced)</button>
          </div>
        </div>
        <div class="field">
          <label>Cloudflare (optional) <span id="s-cf-badge" class="unset-badge"></span></label>
          <p style="margin:0 0 10px;opacity:.75">Only for the <code>cloudfish</code> recon tool — passive subdomain discovery via Cloudflare's DNS scanner, an extra source alongside subfinder/amass. Needs a <b>global</b> API key (high privilege); leave blank if you don't use it. Stored in <code>~/.cloudflare</code>.</p>
          <div class="key-row">
            <input id="s-cf-key" type="password" placeholder="Global API key" />
          </div>
          <div class="row" style="margin-top:8px">
            <input id="s-cf-email" placeholder="Account email" style="flex:1" />
            <input id="s-cf-acct" placeholder="Account ID" style="flex:1" />
          </div>
        </div>
      </div>

      <div class="tab-pane" data-pane="agent">
        <div class="field">
          <label>Personality</label>
          <select id="s-pers">
            ${personalities.map((p) => `<option value="${p}" ${state.personality === p || (!state.personality && p === "") ? "selected" : ""}>${p === "" ? "Default" : p[0].toUpperCase() + p.slice(1)}</option>`).join("")}
          </select>
        </div>
        <div class="field">
          <label>Your standing instructions <span class="unset-badge">optional</span></label>
          <p style="margin:0 0 8px">Short, always-on notes about <b>you and how you work</b> — added to every chat so you don't repeat them. This is <i>not</i> the system prompt (that's below); it's your personal preferences layered on top. Leave blank if you have none.</p>
          <textarea id="s-custom" rows="5" placeholder="Examples:
• My handle is bishop; sign reports as such.
• Rules of engagement: only touch in-scope hosts; never run destructive payloads without asking.
• Prefer nuclei + httpx-toolkit; avoid noisy full-port masscan unless I say so.
• Report findings as: severity, evidence, repro, impact, fix.">${escapeHtml(state.customInstructions)}</textarea>
          <p style="margin:6px 0 0;opacity:.7">Followed on every turn unless it conflicts with the core security mandate.</p>
        </div>
        <div class="field">
          <label>System prompt <span id="s-persona-badge" class="unset-badge"></span></label>
          <p style="margin:0 0 8px">The full cybersecurity-agent system prompt. Edit it to tune behaviour, add methodology, or document new CLI tools you bundled. Applies to the next message. <b>Reset</b> restores the built-in default.</p>
          <textarea id="s-persona" class="mono-edit" rows="14" spellcheck="false" placeholder="Loading…"></textarea>
          <div class="row" style="margin-top:8px">
            <button id="s-persona-save" class="ghost">Save system prompt</button>
            <button id="s-persona-reset" class="ghost">Reset to default</button>
          </div>
        </div>
      </div>

      <div class="tab-pane" data-pane="advanced">
        <div class="field">
          <label><input type="checkbox" id="s-kali" ${state.kaliMode ? "checked" : ""} style="width:auto;margin-right:6px;vertical-align:middle" /> Kali container mode <span id="s-kali-status" class="unset-badge"></span></label>
          <p style="margin:6px 0 0">When on, the Kali container starts <b>automatically</b> and the agent runs tools via <code>docker exec hacksor-kali</code> (working dir mounted). Requires Docker.</p>
        </div>
        <div class="field">
          <label>Runtime Dockerfile <span id="s-df-badge" class="unset-badge"></span></label>
          <p style="margin:0 0 8px">The recipe for the bundled runtime image. Add packages / CLI tools here, then <b>Rebuild</b> to bake them in (a few minutes). Remember to document new tools in the System prompt (Agent tab) so the agent uses them. <b>Reset</b> restores the built-in default.</p>
          <textarea id="s-dockerfile" class="mono-edit" rows="16" spellcheck="false" placeholder="Loading…"></textarea>
          <div class="row" style="margin-top:8px">
            <button id="s-df-save" class="ghost">Save Dockerfile</button>
            <button id="s-df-reset" class="ghost">Reset to default</button>
            <button id="s-rt-build" class="ghost">Rebuild image now</button>
          </div>
          <p style="margin:6px 0 0">In Docker mode the image otherwise downloads/builds automatically at startup. Rebuild recreates the container on the new image.</p>
        </div>
        <div class="field">
          <label>Vulnerability intel <span id="s-intel-status" class="unset-badge"></span></label>
          <p style="margin:6px 0 0">nuclei templates + Exploit-DB refresh <b>automatically</b> each session; the agent also pulls live CVE intel (NVD, cvemap/KEV, EPSS, GitHub PoCs) on demand and can craft a PoC from a CVE.</p>
        </div>
      </div>

      <div class="actions">
        ${force ? "" : '<button id="s-cancel" class="ghost">Cancel</button>'}
        <button id="s-save" class="primary">Save</button>
      </div>
    </div>`;
  app.appendChild(bg);
  const orInput = bg.querySelector("#s-or") as HTMLInputElement;
  const vcInput = bg.querySelector("#s-vc") as HTMLInputElement;
  const persSel = bg.querySelector("#s-pers") as HTMLSelectElement;
  const dirInput = bg.querySelector("#s-dir") as HTMLInputElement;
  bg.querySelector("#s-pick")!.addEventListener("click", async () => {
    const d = await invoke<string | null>("pick_directory");
    if (d) dirInput.value = d;
  });
  const clearKey = async (which: "openrouter_api_key" | "vercel_api_key") => {
    await invoke("save_settings", { args: { [which]: "" } });
    const s = await invoke<SettingsView>("get_settings");
    state.keys = { openrouter: s.has_openrouter_key, vercel: s.has_vercel_key };
    bg.remove();
    document.removeEventListener("keydown", onEsc);
    openSettings(force);
    loadModels();
  };
  bg.querySelector("#s-or-clear")?.addEventListener("click", () => clearKey("openrouter_api_key"));
  bg.querySelector("#s-vc-clear")?.addEventListener("click", () => clearKey("vercel_api_key"));
  // Enter in a key field saves just that key and shows it as set (badge → Saved,
  // masked placeholder), without discarding other unsaved fields in the modal.
  const saveKeyInline = async (which: "openrouter_api_key" | "vercel_api_key", inputEl: HTMLInputElement) => {
    const val = inputEl.value.trim();
    if (!val) return;
    inputEl.disabled = true;
    try {
      await invoke("save_settings", { args: { [which]: val } });
      const s = await invoke<SettingsView>("get_settings");
      state.keys = { openrouter: s.has_openrouter_key, vercel: s.has_vercel_key };
      inputEl.value = "";
      inputEl.classList.add("saved");
      inputEl.placeholder = "•••••••••••••• — leave blank to keep";
      const badge = inputEl.closest(".field")?.querySelector(".unset-badge, .saved-badge") as HTMLElement | null;
      if (badge) { badge.textContent = "✓ Saved"; badge.className = "saved-badge"; }
      loadModels();
    } catch (e) {
      alert(String(e));
    } finally {
      inputEl.disabled = false;
    }
  };
  orInput.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); saveKeyInline("openrouter_api_key", orInput); } });
  vcInput.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); saveKeyInline("vercel_api_key", vcInput); } });
  // Cloudflare creds (for cloudfish): populate current state from get_settings.
  const cfKey = bg.querySelector("#s-cf-key") as HTMLInputElement;
  const cfEmail = bg.querySelector("#s-cf-email") as HTMLInputElement;
  const cfAcct = bg.querySelector("#s-cf-acct") as HTMLInputElement;
  const cfBadge = bg.querySelector("#s-cf-badge") as HTMLElement;
  invoke<SettingsView>("get_settings").then((s) => {
    if (s.cloudflare_email) cfEmail.value = s.cloudflare_email;
    if (s.cloudflare_account_id) cfAcct.value = s.cloudflare_account_id;
    if (s.has_cloudflare) { cfKey.placeholder = "•••••••••• — leave blank to keep"; cfBadge.textContent = "· configured"; cfBadge.className = "saved-badge"; }
  }).catch(() => {});
  // Theme (live-applies on change; persisted by applyTheme).
  bg.querySelector("#s-theme")?.addEventListener("change", (e) =>
    applyTheme((e.target as HTMLSelectElement).value));
  // Runtime services: mode selector + read-only status.
  const svcModeSel = bg.querySelector("#s-services-mode") as HTMLSelectElement;
  const svcEl = bg.querySelector("#s-services") as HTMLElement;
  const dot = (on: boolean) => `<span class="svc-dot ${on ? "on" : ""}"></span>`;
  const svcState = (running: boolean, port: number, mode: string) =>
    running ? `running · 127.0.0.1:${port}` : (mode === "always_on" ? "stopped" : "starts on first use");
  const refreshServices = async () => {
    const s = await invoke<ServicesStatus>("services_status").catch(() => null);
    if (!s) { svcEl.innerHTML = `<span class="unset-badge">Unavailable</span>`; return; }
    svcModeSel.value = s.mode === "always_on" ? "always_on" : "on_demand";
    if (!s.docker) {
      svcEl.innerHTML = `<span class="unset-badge">Docker runtime only — switch Runtime to Docker to run these.</span>`;
      return;
    }
    svcEl.innerHTML =
      `<div class="svc-row">${dot(s.proxy_running)}<b>Intercepting proxy</b>` +
      `<span class="svc-state">${svcState(s.proxy_running, s.proxy_port, s.mode)}</span></div>` +
      `<div class="svc-row">${dot(s.browser_running)}<b>Stealth browser</b>` +
      `<span class="svc-state">${svcState(s.browser_running, s.browser_port, s.mode)}</span></div>`;
  };
  // Persist the mode immediately on change (no restart; affects next runtime prep).
  svcModeSel?.addEventListener("change", async () => {
    await invoke("save_settings", { args: { services_mode: svcModeSel.value } }).catch((e) => alert(String(e)));
  });
  refreshServices();
  // Tab switching.
  bg.querySelectorAll(".tab-btn").forEach((tb) => tb.addEventListener("click", () => {
    const tab = (tb as HTMLElement).dataset.tab;
    bg.querySelectorAll(".tab-btn").forEach((x) => x.classList.toggle("active", x === tb));
    bg.querySelectorAll(".tab-pane").forEach((p) => (p as HTMLElement).classList.toggle("active", (p as HTMLElement).dataset.pane === tab));
  }));
  const kaliStatusEl = bg.querySelector("#s-kali-status") as HTMLElement;
  const refreshKali = async () => {
    const st = await invoke<{ docker: boolean; running: boolean }>("kali_status").catch(() => ({ docker: false, running: false }));
    kaliStatusEl.textContent = !st.docker ? "· Docker not found" : st.running ? "· running (auto)" : "· starts on first message";
    kaliStatusEl.className = st.running ? "saved-badge" : "unset-badge";
  };
  refreshKali();
  const rtStatusEl = bg.querySelector("#s-rt-status") as HTMLElement;
  const refreshRt = async () => {
    const st = await invoke<{ docker: boolean; image: boolean; running: boolean }>("runtime_status").catch(() => ({ docker: false, image: false, running: false }));
    rtStatusEl.textContent = !st.docker ? "· Docker not found" : st.running ? "· container running" : st.image ? "· image built" : "· image not built";
    rtStatusEl.className = st.image ? "saved-badge" : "unset-badge";
  };
  refreshRt();
  bg.querySelector("#s-rt-build")!.addEventListener("click", async (e) => {
    const btn = e.target as HTMLButtonElement;
    const label = btn.textContent;
    btn.textContent = "Rebuilding… (a few minutes)";
    btn.disabled = true;
    try {
      await invoke("rebuild_runtime");
      btn.textContent = label;
    } catch (err) {
      rtStatusEl.textContent = "· " + String(err).slice(0, 80);
    } finally {
      btn.disabled = false;
      refreshRt();
    }
  });

  // Editable Dockerfile + system prompt. Load current content, and wire
  // Save/Reset. Empty content on save reverts to the built-in default.
  type Editable = { text: string; is_custom: boolean };
  const wireEditor = (cmd: string, saveCmd: string, ta: HTMLTextAreaElement, badge: HTMLElement, saveBtn: HTMLElement, resetBtn: HTMLElement, lang?: string) => {
    const setBadge = (custom: boolean) => {
      badge.textContent = custom ? "· customized" : "· default";
      badge.className = custom ? "saved-badge" : "unset-badge";
    };
    const hl = lang ? attachHighlight(ta, lang) : null;
    invoke<Editable>(cmd).then((v) => { ta.value = v.text; setBadge(v.is_custom); hl?.(); }).catch(() => { ta.placeholder = "(failed to load)"; });
    saveBtn.addEventListener("click", async () => {
      const t = saveBtn.textContent;
      saveBtn.textContent = "Saved ✓";
      try { await invoke(saveCmd, { content: ta.value }); setBadge(!!ta.value.trim()); }
      catch (err) { saveBtn.textContent = "Save failed"; console.error(err); }
      setTimeout(() => (saveBtn.textContent = t), 1200);
    });
    resetBtn.addEventListener("click", async () => {
      await invoke(saveCmd, { content: "" }).catch(() => {});
      const v = await invoke<Editable>(cmd).catch(() => ({ text: "", is_custom: false }));
      ta.value = v.text; setBadge(v.is_custom); hl?.();
    });
  };
  wireEditor("get_dockerfile", "save_dockerfile",
    bg.querySelector("#s-dockerfile") as HTMLTextAreaElement,
    bg.querySelector("#s-df-badge") as HTMLElement,
    bg.querySelector("#s-df-save") as HTMLElement,
    bg.querySelector("#s-df-reset") as HTMLElement,
    "dockerfile");
  wireEditor("get_persona", "save_persona",
    bg.querySelector("#s-persona") as HTMLTextAreaElement,
    bg.querySelector("#s-persona-badge") as HTMLElement,
    bg.querySelector("#s-persona-save") as HTMLElement,
    bg.querySelector("#s-persona-reset") as HTMLElement);
  const ocxStatusEl = bg.querySelector("#s-ocx-status") as HTMLElement;
  const refreshOcx = async () => {
    const st = await invoke<{ installed: boolean; running: boolean }>("opencodex_status").catch(() => ({ installed: false, running: false }));
    ocxStatusEl.textContent = !st.installed ? "· not installed" : st.running ? "· running" : "· installed (stopped)";
    ocxStatusEl.className = st.running ? "saved-badge" : "unset-badge";
  };
  refreshOcx();
  bg.querySelector("#s-ocx-setup")!.addEventListener("click", async () => {
    await invoke("open_opencodex_setup").catch((e) => { ocxStatusEl.textContent = "· " + e; });
    setTimeout(refreshOcx, 1500);
  });

  // Provider wrapper: add a key for any upstream via OpenCodex.
  const OCX_PROVIDERS: { id: string; name: string; hint: string; keyless?: boolean }[] = [
    // OpenRouter + Vercel have their own key fields above; not repeated here.
    { id: "openai", name: "OpenAI (GPT)", hint: "sk-..." },
    { id: "anthropic", name: "Anthropic (Claude)", hint: "sk-ant-..." },
    { id: "google", name: "Google Gemini", hint: "AIza..." },
    { id: "groq", name: "Groq", hint: "gsk_..." },
    { id: "deepseek", name: "DeepSeek", hint: "sk-..." },
    { id: "together", name: "Together AI", hint: "..." },
    { id: "xai", name: "xAI (Grok)", hint: "xai-..." },
    { id: "ollama", name: "Ollama (local)", hint: "no key needed", keyless: true },
  ];
  const provList = bg.querySelector("#s-prov-list") as HTMLElement;
  provList.innerHTML = "";
  for (const p of OCX_PROVIDERS) {
    const row = el("div", "prov-row");
    row.innerHTML =
      `<span class="prov-name">${escapeHtml(p.name)}</span>` +
      (p.keyless
        ? `<span class="prov-note">${escapeHtml(p.hint)}</span>`
        : `<input class="prov-key" type="password" placeholder="${escapeHtml(p.hint)}" />`) +
      `<button class="prov-add ghost">Add</button>`;
    const btn = row.querySelector(".prov-add") as HTMLButtonElement;
    btn.addEventListener("click", async () => {
      const key = (row.querySelector(".prov-key") as HTMLInputElement | null)?.value?.trim() ?? "";
      if (!p.keyless && !key) { btn.textContent = "Enter key"; setTimeout(() => (btn.textContent = "Add"), 1200); return; }
      btn.textContent = "Adding…"; btn.disabled = true;
      try {
        await invoke("ocx_add_provider", { provider: p.id, key });
        btn.textContent = "✓ Added";
        const inp = row.querySelector(".prov-key") as HTMLInputElement | null; if (inp) inp.value = "";
        refreshOcx();
      } catch (e) {
        btn.textContent = "Failed"; row.title = String(e);
      } finally {
        btn.disabled = false;
        setTimeout(() => (btn.textContent = "Add"), 1600);
      }
    });
    provList.appendChild(row);
  }
  const close = () => {
    document.removeEventListener("keydown", onEsc);
    bg.remove();
  };
  const onEsc = (e: KeyboardEvent) => {
    if (e.key === "Escape") { e.preventDefault(); close(); }
  };
  document.addEventListener("keydown", onEsc);
  bg.addEventListener("mousedown", (e) => { if (e.target === bg) close(); });
  bg.querySelector("#s-close")!.addEventListener("click", close);
  bg.querySelector("#s-cancel")?.addEventListener("click", close);
  bg.querySelector("#s-save")!.addEventListener("click", async () => {
    const prevRuntime = state.runtime;
    const newRuntime = (bg.querySelector("#s-runtime") as HTMLSelectElement).value;
    await invoke("save_settings", {
      args: {
        openrouter_api_key: orInput.value ? orInput.value : null,
        vercel_api_key: vcInput.value ? vcInput.value : null,
        personality: persSel.value,
        custom_instructions: (bg.querySelector("#s-custom") as HTMLTextAreaElement).value,
        working_dir: dirInput.value || null,
        kali_mode: (bg.querySelector("#s-kali") as HTMLInputElement).checked,
        runtime: newRuntime,
        // Cloudflare (cloudfish): blank key keeps the saved one; email/account
        // are sent as-is so edits and clears both apply.
        cloudflare_api_key: cfKey.value ? cfKey.value : null,
        cloudflare_email: cfEmail.value,
        cloudflare_account_id: cfAcct.value,
      },
    });
    const s = await invoke<SettingsView>("get_settings");
    state.keys = { openrouter: s.has_openrouter_key, vercel: s.has_vercel_key };
    state.personality = s.personality;
    state.customInstructions = s.custom_instructions ?? "";
    state.workingDir = s.working_dir;
    state.kaliMode = s.kali_mode;
    state.runtime = s.runtime;
    updateHint();
    close();
    await loadModels();
    loadRecents();
    // Provision the runtime up front (download/build + start) so it's ready
    // before the first message. Idempotent, so safe whenever Docker is selected.
    void prevRuntime;
    if (state.runtime === "docker") prepareRuntime();
  });
}

// ---------------------------------------------------------------- render helpers

function renderMd(s: string): string {
  return marked.parse(s) as string;
}
function renderDiff(diff: string): string {
  return diff.split("\n").map((line) => {
    const e = escapeHtml(line);
    if (line.startsWith("+")) return `<span class="add">${e}</span>`;
    if (line.startsWith("-")) return `<span class="del">${e}</span>`;
    if (line.startsWith("@@")) return `<span class="hunk">${e}</span>`;
    return e;
  }).join("\n");
}

// ---------------------------------------------------------------- custom right-click menu

function popupAt(x: number, y: number, panel: HTMLElement) {
  closeMenu();
  panel.classList.add("menu");
  document.body.appendChild(panel);
  const pw = panel.offsetWidth, ph = panel.offsetHeight;
  panel.style.left = Math.min(x, window.innerWidth - pw - 12) + "px";
  panel.style.top = Math.min(y, window.innerHeight - ph - 12) + "px";
  openPanel = panel;
  setTimeout(() => {
    document.addEventListener("mousedown", onDocDown, true);
    document.addEventListener("keydown", onMenuEsc, true);
  }, 0);
}

window.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  const target = e.target as HTMLElement;
  const panel = el("div", "menu-list");
  const add = (label: string, fn: () => void, danger = false) => {
    const b = el("button", "menu-item ctx" + (danger ? " danger" : ""), label);
    b.onclick = () => { closeMenu(); fn(); };
    panel.appendChild(b);
  };
  const sess = target.closest(".item2") as HTMLElement | null;
  const bubble = target.closest(".bubble") as HTMLElement | null;
  const editable = target.closest("textarea, input") as HTMLElement | null;

  if (editable) {
    add("Paste", async () => {
      try {
        const t = await navigator.clipboard.readText();
        const ta = editable as HTMLTextAreaElement;
        const start = ta.selectionStart ?? ta.value.length;
        ta.value = ta.value.slice(0, start) + t + ta.value.slice(ta.selectionEnd ?? start);
        ta.dispatchEvent(new Event("input"));
      } catch {}
    });
    add("Select all", () => (editable as HTMLTextAreaElement).select());
  } else if (sess) {
    const s = state.sessions.find((x) => x.id === sess.dataset.id);
    add("New chat", () => newSession());
    if (s?.archived) {
      add("Restore", () => unarchiveSession(sess.dataset.id!, true));
      add("Delete permanently", () => deleteSession(sess.dataset.id!), true);
    } else if (s) {
      const tid = sess.dataset.tid;
      add(tid && pinnedIds.has(tid) ? "Unpin" : "Pin", () => togglePin(tid));
      add("Rename", () => renameSession(s.id));
      add("Export to Markdown", () => exportChat(s));
      add("Archive chat", () => archiveSession(s.id));
    } else {
      const tid = sess.dataset.tid;
      add(tid && pinnedIds.has(tid) ? "Unpin" : "Pin", () => togglePin(tid));
      if (tid) add("Archive chat", () => { archivedIds.add(tid); saveSet("hacksor-archived", archivedIds); renderSidebar(); });
    }
  } else if (bubble) {
    add("Copy", () => navigator.clipboard?.writeText(bubble.innerText).catch(() => {}));
    add("New chat", () => newSession());
  } else {
    add("New chat", () => newSession());
    add("New security sub-task", () => newSession("task"));
    add("New validation task", () => newSession("validate"));
    add("Toggle sidebar", () => toggleSidebar());
    add("Settings", () => openSettings());
  }
  popupAt(e.clientX, e.clientY, panel);
});

boot().catch((e) => addError(activeSession(), String(e)));
