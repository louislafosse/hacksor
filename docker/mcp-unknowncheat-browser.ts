/**
 * Camoufox-backed browser for mcp-unknowncheat, replacing its Chrome layer.
 *
 * Upstream drives its own Chrome through puppeteer-real-browser and then waits
 * for a HUMAN to clear Cloudflare in a visible window. Measured in this
 * runtime, that path cannot work: headless Chromium ends in
 * "CloudflareBlockError: Challenge stayed open in Chrome", and so does a
 * headful Chromium under Xvfb with the wait raised to 90 s. There is no window
 * for anyone to click in, and the challenge is decided on browser fingerprint
 * rather than on whether a window exists.
 *
 * Camoufox is a stealth Firefox that passes those challenges unattended, and
 * this image already runs it as a local HTTP service for the agent's own
 * browser tools. So this module keeps every export upstream's modules import
 * and serves them from that service instead. Nothing above this file changes:
 * the tools still receive a `page`, still call `evaluate`/`content`/`type`/
 * `click`, and still parse HTML with cheerio.
 *
 * Two upstream behaviours are deliberately dropped:
 *   - cookies.json persistence, because a Camoufox profile is keyed by userId
 *     and already persists cookies and fingerprint across runs — which is what
 *     makes a cleared challenge stay cleared.
 *   - CDP (`createCDPSession`), which is Chrome-only. Firefox has no
 *     equivalent, so the one caller (download_file, setting a download
 *     directory) gets a stub that resolves and a warning on stderr.
 */

import { isApacheNotFoundPage, normalizeThreadUrl } from "./forum-url.js";

const BASE = (process.env.CAMOFOX_URL ?? "http://127.0.0.1:9377").replace(/\/+$/, "");
/**
 * One stable profile, so a cleared challenge and a forum login survive
 * restarts. The service keys a profile on userId AND sessionKey and rejects a
 * tab request that omits either ("userId and sessionKey required"), so both are
 * fixed values rather than the random session key its own MCP client generates.
 */
const USER_ID = process.env.UC_CAMOFOX_USER ?? "unknowncheats";
const SESSION_KEY = process.env.UC_CAMOFOX_SESSION ?? "unknowncheats";
const API_KEY = process.env.CAMOFOX_API_KEY ?? "";

const NAV_TIMEOUT = 45_000;
const EVAL_TIMEOUT = 30_000;
/** The service caps one evaluate result at 1 MiB; read page HTML in slices. */
const EVAL_CHUNK = 262_144;
const CF_WAIT_MS = Number(process.env.UC_CF_WAIT_MS ?? 45_000);
const CLOUDFLARE_INDICATORS = ["Just a moment", "cf-browser-verification", "Checking your browser"];

const ALLOWED_HOSTS = new Set(["www.unknowncheats.me", "unknowncheats.me"]);

/** Unchanged from upstream: this server only ever opens one forum. */
export function validateUrl(url: string): void {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    throw new Error(`Invalid URL: ${url}`);
  }
  if (parsed.protocol !== "https:" && parsed.protocol !== "http:") {
    throw new Error(`Blocked URL scheme: ${parsed.protocol} — only http/https allowed`);
  }
  if (!ALLOWED_HOSTS.has(parsed.hostname)) {
    throw new Error(`Blocked URL host: ${parsed.hostname} — only unknowncheats.me is allowed`);
  }
}

const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, Math.max(0, ms)));

let daemonEnsured = false;
let daemonStartTried = false;

const isLoopback = (): boolean => /\/\/(127\.0\.0\.1|localhost|\[?::1\]?)(:|\/|$)/.test(BASE);

async function pingDaemon(): Promise<boolean> {
  try {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 1_500);
    const res = await fetch(`${BASE}/health`, { signal: controller.signal });
    clearTimeout(timer);
    return res.ok;
  } catch {
    return false;
  }
}

/**
 * Start the Camoufox daemon if it is not up, then wait for it.
 *
 * The daemon is deliberately LAZY in this image — Firefox is heavy, so nothing
 * starts it with the container and camofox-mcp is patched to bring it up on its
 * own first request. This server talks to the same service directly, so without
 * the same courtesy its first tool call failed with ECONNREFUSED on
 * 127.0.0.1:9377 whenever no browser tool had run yet in the session, and that
 * reads as "the MCP server is broken" rather than "the browser is not up".
 *
 * `--background` matters: the daemon started from a shell with a plain `&` dies
 * with that shell, so a health probe succeeds and the next call fails again.
 * The RAM caps match the ones camofox-mcp's patch uses.
 */
async function ensureDaemon(): Promise<void> {
  if (daemonEnsured) return;
  if (await pingDaemon()) {
    daemonEnsured = true;
    return;
  }
  if (!isLoopback()) {
    throw new Error(`Camoufox service at ${BASE} is not answering, and it is not local to start.`);
  }
  if (!daemonStartTried) {
    daemonStartTried = true;
    const port = (BASE.match(/:(\d+)/) ?? [])[1] ?? "9377";
    console.error(`[browser] Camoufox daemon is down; starting it on port ${port}`);
    try {
      const { spawn } = await import("node:child_process");
      spawn("camofox-browser", ["server", "start", "--port", port, "--background"], {
        env: {
          ...process.env,
          CAMOFOX_HOST: "127.0.0.1",
          CAMOFOX_AUTH_MODE: "disabled",
          CAMOFOX_MAX_SESSIONS: "3",
          CAMOFOX_IDLE_TIMEOUT_MS: "300000",
        },
        detached: true,
        stdio: "ignore",
      }).unref();
    } catch (error) {
      console.error(`[browser] could not spawn camofox-browser: ${(error as Error).message}`);
    }
  }
  for (let i = 0; i < 60; i++) {
    await sleep(750);
    if (await pingDaemon()) {
      daemonEnsured = true;
      console.error("[browser] Camoufox daemon is up");
      return;
    }
  }
  throw new Error(
    `Camoufox service never came up at ${BASE}. Start it by hand with ` +
      "`camofox-browser server start --port 9377 --background` and check /tmp for its log.",
  );
}

async function api(path: string, init: { method?: string; body?: unknown } = {}): Promise<any> {
  await ensureDaemon();
  const headers: Record<string, string> = { "content-type": "application/json" };
  if (API_KEY) headers["x-camofox-api-key"] = API_KEY;
  let response: Response;
  try {
    response = await fetch(`${BASE}${path}`, {
      method: init.method ?? "GET",
      headers,
      ...(init.body === undefined ? {} : { body: JSON.stringify(init.body) }),
    });
  } catch (error) {
    // The daemon can be recycled between calls (idle timeout, a crash); bring
    // it back once rather than failing the tool.
    daemonEnsured = false;
    daemonStartTried = false;
    await ensureDaemon();
    try {
      response = await fetch(`${BASE}${path}`, {
        method: init.method ?? "GET",
        headers,
        ...(init.body === undefined ? {} : { body: JSON.stringify(init.body) }),
      });
    } catch (retryError) {
      throw new Error(
        `Camoufox service unreachable at ${BASE} (${(retryError as Error).message}, ` +
          `first attempt: ${(error as Error).message}).`,
      );
    }
  }
  const text = await response.text();
  if (!response.ok) {
    throw new Error(`Camoufox ${init.method ?? "GET"} ${path} failed: HTTP ${response.status} ${text.slice(0, 400)}`);
  }
  if (!text) return {};
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

/** The surface upstream's tools actually use, served by Camoufox. */
class CamofoxPage {
  readonly tabId: string;
  private currentUrl: string;
  private cdpWarned = false;

  constructor(tabId: string, url: string) {
    this.tabId = tabId;
    this.currentUrl = url;
  }

  static async open(): Promise<CamofoxPage> {
    const created = await api("/tabs", {
      method: "POST",
      body: { userId: USER_ID, sessionKey: SESSION_KEY },
    });
    const tabId = created.tabId ?? created.id ?? created.tab?.id;
    if (!tabId) throw new Error(`Camoufox did not return a tab id: ${JSON.stringify(created).slice(0, 300)}`);
    return new CamofoxPage(tabId, created.url ?? "about:blank");
  }

  /** Raw evaluate: returns the value, throws the page-side error message. */
  private async evalRaw(expression: string, timeout = EVAL_TIMEOUT): Promise<any> {
    const res = await api(`/tabs/${encodeURIComponent(this.tabId)}/evaluate`, {
      method: "POST",
      body: { expression, userId: USER_ID, timeout },
    });
    if (res && res.ok === false) {
      throw new Error(`Page evaluate failed: ${res.errorType ?? "error"}: ${res.error ?? "unknown"}`);
    }
    return res?.result;
  }

  /**
   * puppeteer-compatible evaluate: a function (with JSON-serializable args) or
   * an expression string. The function is stringified and applied in the page,
   * exactly as puppeteer does it, so upstream's callbacks work unchanged.
   */
  async evaluate(fn: unknown, ...args: unknown[]): Promise<any> {
    const expression =
      typeof fn === "function"
        ? `(${fn.toString()}).apply(null, ${JSON.stringify(args)})`
        : `(${String(fn)})`;
    return this.evalRaw(expression);
  }

  /** Whole-document HTML, read in slices so the 1 MiB result cap can't truncate it. */
  async content(): Promise<string> {
    const length = Number(await this.evalRaw("document.documentElement.outerHTML.length"));
    if (!Number.isFinite(length) || length <= 0) return "";
    if (length <= EVAL_CHUNK) return String(await this.evalRaw("document.documentElement.outerHTML"));
    let html = "";
    for (let offset = 0; offset < length; offset += EVAL_CHUNK) {
      const slice = await this.evalRaw(
        `document.documentElement.outerHTML.slice(${offset}, ${offset + EVAL_CHUNK})`,
      );
      html += String(slice ?? "");
    }
    return html;
  }

  async goto(url: string, _options?: unknown): Promise<null> {
    const res = await api(`/tabs/${encodeURIComponent(this.tabId)}/navigate`, {
      method: "POST",
      body: { url, userId: USER_ID },
    });
    this.currentUrl = res?.url ?? url;
    return null;
  }

  /** Synchronous in puppeteer, so callers read it without awaiting. */
  url(): string {
    return this.currentUrl;
  }

  async title(): Promise<string> {
    return String((await this.evalRaw("document.title")) ?? "");
  }

  async type(selector: string, text: string, _options?: unknown): Promise<void> {
    await api(`/tabs/${encodeURIComponent(this.tabId)}/type`, {
      method: "POST",
      body: { selector, text, userId: USER_ID },
    });
  }

  async click(selector: string, _options?: unknown): Promise<void> {
    await api(`/tabs/${encodeURIComponent(this.tabId)}/click`, {
      method: "POST",
      body: { selector, userId: USER_ID },
    });
  }

  /**
   * Upstream races this against a click or a form submit. There is no
   * navigation event to subscribe to over HTTP, so poll the page's own URL and
   * readyState. It resolves rather than rejecting on timeout: the caller reads
   * the resulting HTML next and can judge it, which beats failing a login over
   * a navigation that simply took longer than expected.
   */
  async waitForNavigation(options?: { timeout?: number }): Promise<null> {
    const before = this.currentUrl;
    const stopAt = Date.now() + Math.max(1_000, options?.timeout ?? NAV_TIMEOUT);
    while (Date.now() < stopAt) {
      await sleep(400);
      try {
        const state = await this.evalRaw("[location.href, document.readyState]");
        const href = Array.isArray(state) ? String(state[0]) : undefined;
        const ready = Array.isArray(state) ? String(state[1]) : undefined;
        if (href && href !== before && ready !== "loading") {
          this.currentUrl = href;
          return null;
        }
      } catch {
        // A navigation in flight can tear the execution context down; retry.
      }
    }
    console.error("[browser] waitForNavigation: no URL change observed; continuing with the current page");
    return null;
  }

  async cookies(): Promise<any[]> {
    const res = await api(
      `/tabs/${encodeURIComponent(this.tabId)}/cookies?userId=${encodeURIComponent(USER_ID)}`,
    );
    return Array.isArray(res) ? res : (res?.cookies ?? []);
  }

  async setCookie(...cookies: any[]): Promise<void> {
    if (cookies.length === 0) return;
    await api(`/sessions/${encodeURIComponent(USER_ID)}/cookies`, {
      method: "POST",
      body: { cookies, tabId: this.tabId },
    });
  }

  /** Chrome-only. Resolve so download_file's other paths still run. */
  async createCDPSession(): Promise<{ send: (...a: unknown[]) => Promise<any>; detach: () => Promise<void> }> {
    if (!this.cdpWarned) {
      this.cdpWarned = true;
      console.error(
        "[browser] CDP is unavailable on Camoufox (Firefox); download directory control is skipped.",
      );
    }
    return { send: async () => ({}), detach: async () => {} };
  }

  async close(): Promise<void> {
    await api(`/tabs/${encodeURIComponent(this.tabId)}`, { method: "DELETE", body: { userId: USER_ID } }).catch(
      () => undefined,
    );
  }
}

let page: CamofoxPage | null = null;
let sessionTail: Promise<void> = Promise.resolve();

/**
 * Unchanged from upstream: a tool owns the shared page for its whole workflow,
 * because a navigation-only lock would let another tool navigate away between
 * this tool's navigate and its content()/evaluate().
 */
export function withBrowserSession<T>(operation: () => Promise<T>, maxQueueWaitMs = 10_000): Promise<T> {
  let started = false;
  let cancelled = false;
  let timer: ReturnType<typeof setTimeout>;
  const queueTimeout = new Promise<never>((_, reject) => {
    timer = setTimeout(() => {
      if (started) return;
      cancelled = true;
      reject(new Error(`Browser busy: queue wait exceeded ${maxQueueWaitMs} ms`));
    }, maxQueueWaitMs);
  });
  const result = sessionTail.then(() => {
    if (cancelled) throw new Error("Browser session request cancelled while queued");
    started = true;
    clearTimeout(timer);
    return operation();
  });
  sessionTail = result.then(
    () => undefined,
    () => undefined,
  );
  return Promise.race([result, queueTimeout]);
}

export async function getPage(): Promise<CamofoxPage> {
  if (!page) {
    console.error(`[browser] Opening a Camoufox tab via ${BASE} (profile "${USER_ID}")`);
    page = await CamofoxPage.open();
  }
  return page;
}

export async function ensureFreshBrowser(): Promise<CamofoxPage> {
  if (page) {
    await page.close();
    page = null;
  }
  return getPage();
}

function hasCloudflareChallenge(html: string): boolean {
  return CLOUDFLARE_INDICATORS.some((indicator) => html.includes(indicator));
}

function isPendingPage(html: string): boolean {
  return html.length < 200 || hasCloudflareChallenge(html);
}

/** Turnstile's checkbox, in the order the challenge page tends to expose it. */
const TURNSTILE_SELECTORS = [
  'input[type="checkbox"]',
  "#challenge-stage input",
  "label.ctp-checkbox-label",
  ".cb-lb input",
];

/**
 * Clear a challenge without a human.
 *
 * Camoufox passes most challenges on navigation alone, but a MANAGED one waits
 * for a click on the Turnstile checkbox, which lives in a cross-origin iframe.
 * This image patches camofox-browser's click so a failed page-level selector
 * walks child frames and clicks by bounding-box centre with a real mouse
 * sequence — exactly so that checkbox is reachable. So the loop does all three
 * things rather than only waiting: re-read the page, click the checkbox, and
 * re-navigate every few passes, since clearance usually lands on the next
 * request rather than in the open tab.
 *
 * Waiting alone was measured failing on a subforum page inside a 37 s window.
 */
async function settleChallenge(
  current: CamofoxPage,
  url: string,
  initialHtml: string,
  deadlineAt?: number,
): Promise<string> {
  if (!isPendingPage(initialHtml)) return initialHtml;
  const waitMs = Number.isFinite(CF_WAIT_MS) ? Math.max(0, CF_WAIT_MS) : 45_000;
  const stopAt = Math.min(Date.now() + waitMs, deadlineAt ?? Infinity);
  console.error(
    `[browser] Challenge page; solving it in Camoufox for up to ${Math.ceil(Math.max(0, stopAt - Date.now()) / 1000)}s`,
  );
  let html = initialHtml;
  for (let pass = 0; Date.now() < stopAt; pass++) {
    await sleep(1_500);
    if (pass % 2 === 1) {
      for (const selector of TURNSTILE_SELECTORS) {
        try {
          await current.click(selector);
          break;
        } catch {
          // not this selector, or nothing clickable yet
        }
      }
    }
    if (pass > 0 && pass % 6 === 0) {
      try {
        await current.goto(url);
      } catch {
        // a reload during a challenge redirect can fail; the next pass retries
      }
    }
    try {
      html = await current.content();
      if (!isPendingPage(html)) {
        console.error(`[browser] Challenge cleared after ${pass + 1} pass(es)`);
        return html;
      }
    } catch {
      // keep going; the page may be mid-navigation
    }
  }
  throw new Error(
    "CloudflareBlockError: the challenge did not clear in Camoufox within the wait window. " +
      "Retry — the profile keeps whatever clearance it did get — or raise UC_CF_WAIT_MS.",
  );
}

export async function navigateWithRetry(
  url: string,
  deadlineAt?: number,
): Promise<{ page: CamofoxPage; html: string }> {
  url = normalizeThreadUrl(url);
  validateUrl(url);
  let current = await getPage();
  let retried = false;

  const attempt = async (): Promise<string> => {
    await current.goto(url);
    const html = await settleChallenge(current, url, await current.content(), deadlineAt);
    if (isApacheNotFoundPage(html)) throw new Error(`Forum page not found (HTTP 404): ${url}`);
    return html;
  };

  try {
    return { page: current, html: await attempt() };
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    const recoverable =
      message.includes("Camoufox") ||
      message.includes("tab") ||
      message.includes("evaluate") ||
      message.includes("HTTP 5");
    if (!retried && recoverable && !message.startsWith("CloudflareBlockError")) {
      retried = true;
      console.error("[browser] Tab error, reopening the Camoufox tab and retrying once...");
      current = await ensureFreshBrowser();
      return { page: current, html: await attempt() };
    }
    throw error;
  }
}

export async function closeBrowser(): Promise<void> {
  if (page) {
    await page.close();
    page = null;
  }
}
