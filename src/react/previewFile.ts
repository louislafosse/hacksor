import { marked } from "marked";
import hljs from "highlight.js";

/** Extension → highlight.js language, for the file types an engagement holds. */
const LANGS: Record<string, string> = {
  // scripting and exploit code
  py: "python", py3: "python", pyw: "python",
  sh: "bash", bash: "bash", zsh: "bash", fish: "shell", ksh: "bash", command: "bash",
  ps1: "powershell", psm1: "powershell", bat: "dos", cmd: "dos", vbs: "vbscript",
  rb: "ruby", php: "php", php5: "php", phtml: "php", pl: "perl", pm: "perl",
  lua: "lua", tcl: "tcl", r: "r", groovy: "groovy", scala: "scala", kt: "kotlin",
  swift: "swift", dart: "dart", ex: "elixir", exs: "elixir", erl: "erlang",
  go: "go", rs: "rust", java: "java", cs: "csharp", fs: "fsharp",
  c: "c", h: "c", cpp: "cpp", cc: "cpp", cxx: "cpp", hpp: "cpp", hh: "cpp",
  m: "objectivec", mm: "objectivec", asm: "x86asm", s: "x86asm", nasm: "x86asm",
  js: "javascript", mjs: "javascript", cjs: "javascript", jsx: "javascript",
  ts: "typescript", tsx: "typescript", vue: "xml", svelte: "xml",

  // data and structured output — the bulk of what a scan leaves behind
  json: "json", jsonl: "json", ndjson: "json", json5: "json", geojson: "json",
  har: "json", map: "json", lock: "json",
  yml: "yaml", yaml: "yaml",
  toml: "ini", ini: "ini", conf: "ini", cfg: "ini", properties: "ini",
  env: "bash", editorconfig: "ini",
  xml: "xml", html: "xml", htm: "xml", xhtml: "xml", svg: "xml", plist: "xml",
  rss: "xml", atom: "xml", wsdl: "xml", xsd: "xml", pom: "xml", csproj: "xml",
  nmap: "plaintext", gnmap: "plaintext", xsl: "xml",
  sql: "sql", psql: "sql", graphql: "graphql", gql: "graphql", proto: "protobuf",
  csv: "plaintext", tsv: "plaintext",

  // web and styling
  css: "css", scss: "scss", sass: "scss", less: "less", styl: "stylus",

  // infra and build
  dockerfile: "dockerfile", containerfile: "dockerfile",
  tf: "terraform", tfvars: "terraform", hcl: "terraform",
  mk: "makefile", cmake: "cmake", gradle: "groovy",
  service: "ini", nginx: "nginx", vcl: "plaintext",

  // diffs, logs, keys, misc
  diff: "diff", patch: "diff",
  log: "accesslog", out: "plaintext", err: "plaintext", txt: "plaintext",
  pem: "plaintext", crt: "plaintext", cer: "plaintext", key: "plaintext",
  http: "http", rest: "http", curl: "bash",
  reg: "ini", desktop: "ini", gitignore: "plaintext", gitconfig: "ini",
};

/** Files identified by NAME rather than extension. */
const BY_NAME: Record<string, string> = {
  dockerfile: "dockerfile", containerfile: "dockerfile",
  makefile: "makefile", gnumakefile: "makefile", rakefile: "ruby",
  gemfile: "ruby", vagrantfile: "ruby", brewfile: "ruby",
  ".gitignore": "plaintext", ".dockerignore": "plaintext",
  ".bashrc": "bash", ".zshrc": "bash", ".profile": "bash", ".bash_profile": "bash",
  ".gitconfig": "ini", ".npmrc": "ini", ".editorconfig": "ini", ".env": "bash",
  "requirements.txt": "plaintext", "cargo.lock": "ini", "go.mod": "go", "go.sum": "plaintext",
};

/** Files whose whole point is their rendered form. */
const MARKDOWN = new Set(["md", "markdown", "mdx"]);

const esc = (s: string) =>
  s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");

/** Highlighting a huge file costs more than it returns; show it plain instead. */
const HIGHLIGHT_MAX = 400_000;

/**
 * Render one engagement file for preview.
 *
 * Notes and reports are markdown and were being shown as raw source — headings
 * as `##`, tables as pipes — which defeats the point of keeping the record in
 * markdown at all. Scripts and scan output get syntax colour, since reading a
 * PoC or a JSON blob unhighlighted is the thing this panel exists to avoid.
 */
export function previewFile(rel: string, body: string): { html: string; kind: "markdown" | "code" | "text" } {
  const name = rel.split("/").pop() ?? rel;
  const ext = (name.includes(".") ? name.split(".").pop() ?? "" : name).toLowerCase();

  if (MARKDOWN.has(ext)) {
    return { html: marked.parse(body) as string, kind: "markdown" };
  }

  if (body.length > HIGHLIGHT_MAX) {
    return { html: `<pre class="pv-code"><code>${esc(body)}</code></pre>`, kind: "text" };
  }

  const lower = name.toLowerCase();
  const lang = BY_NAME[lower] ?? LANGS[ext] ?? "";
  if (lang && lang !== "plaintext" && hljs.getLanguage(lang)) {
    try {
      const out = hljs.highlight(body, { language: lang }).value;
      return { html: `<pre class="pv-code"><code class="hljs language-${lang}">${out}</code></pre>`, kind: "code" };
    } catch {
      /* fall through to plain */
    }
  }
  // No extension to go on (common for scan dumps): let highlight.js guess,
  // but only for something small enough that a wrong guess costs nothing.
  if (!lang && body.length < 40_000) {
    try {
      const auto = hljs.highlightAuto(body);
      if (auto.relevance > 6) {
        return { html: `<pre class="pv-code"><code class="hljs">${auto.value}</code></pre>`, kind: "code" };
      }
    } catch { /* fall through */ }
  }
  return { html: `<pre class="pv-code"><code>${esc(body)}</code></pre>`, kind: "text" };
}
