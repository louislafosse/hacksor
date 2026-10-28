import { useEffect, useMemo, useState } from "react";
import { FileText, FolderOpen, ShieldAlert, ScrollText, Search, ExternalLink } from "lucide-react";
import { Input } from "@/components/ui/input";
import { Separator } from "@/components/ui/separator";
import { cn } from "@/lib/utils";
import { previewFile } from "@/react/previewFile";

export type Finding = {
  id: string; title: string; severity: string; cvss: string; cvss_vector: string;
  owasp: string; url: string; description: string; evidence: string;
  remediation: string; status: string;
};
export type EngagementFile = { rel: string; size: number; modified: number; dir: boolean };
export type EngagementView = {
  dir: string; findings: Finding[]; files: EngagementFile[];
  has_notes: boolean; has_report: boolean; has_residual: boolean; total_bytes: number;
};

const SEV = ["critical", "high", "medium", "low", "info"] as const;
const sevColor: Record<string, string> = {
  critical: "#f05d6c", high: "#ef8a4b", medium: "#d8a657", low: "#51da4c", info: "#8ab4f8",
};

function bytes(n: number) {
  if (n >= 1 << 30) return `${(n / (1 << 30)).toFixed(1)} GB`;
  if (n >= 1 << 20) return `${(n / (1 << 20)).toFixed(1)} MB`;
  if (n >= 1 << 10) return `${Math.round(n / (1 << 10))} KB`;
  return `${n} B`;
}
function ago(sec: number) {
  const d = Date.now() / 1000 - sec;
  if (d < 90) return "just now";
  if (d < 3600) return `${Math.round(d / 60)}m ago`;
  if (d < 86400) return `${Math.round(d / 3600)}h ago`;
  return `${Math.round(d / 86400)}d ago`;
}

function SeverityPill({ severity, cvss }: { severity: string; cvss: string }) {
  const s = (severity || "info").toLowerCase();
  const c = sevColor[s] ?? sevColor.info;
  return (
    <span
      className="shrink-0 rounded-[7px] border px-2 py-[3px] text-[10.5px] font-bold tracking-wide"
      style={{ color: c, background: `color-mix(in srgb, ${c} 16%, transparent)`, borderColor: `color-mix(in srgb, ${c} 38%, transparent)` }}
    >
      {s.toUpperCase()}{cvss ? ` · ${cvss}` : ""}
    </span>
  );
}

/**
 * A read-only window onto the engagement directory: what was found, what will
 * ship, and the evidence behind it.
 *
 * All of it already existed on disk — findings.json, deliverables/report.md,
 * artifacts/ — but reading any of it meant leaving the app for a file manager,
 * which is precisely when you stop checking whether the report matches the
 * work.
 */
export function EngagementPanel({
  view, onOpenFile, onReveal, onClose,
}: {
  view: EngagementView;
  onOpenFile: (rel: string) => Promise<string>;
  /** Hand off to the OS file manager — for binaries, captures and sharing. */
  onReveal: (rel?: string) => void;
  onClose: () => void;
}) {
  const [tab, setTab] = useState<"findings" | "docs" | "artifacts">("findings");
  const [q, setQ] = useState("");
  const [open, setOpen] = useState<string | null>(null);
  const [body, setBody] = useState<string>("");
  const [err, setErr] = useState<string>("");

  useEffect(() => {
    if (!open) return;
    setErr(""); setBody("Loading…");
    onOpenFile(open).then(setBody, (e) => { setBody(""); setErr(String(e)); });
  }, [open, onOpenFile]);

  const counts = useMemo(() => {
    const c: Record<string, number> = {};
    for (const f of view.findings) c[(f.severity || "info").toLowerCase()] = (c[(f.severity || "info").toLowerCase()] ?? 0) + 1;
    return c;
  }, [view.findings]);

  const files = useMemo(() => {
    const ql = q.trim().toLowerCase();
    return view.files.filter((f) => !ql || f.rel.toLowerCase().includes(ql));
  }, [view.files, q]);

  const docs = [
    { rel: "ENGAGEMENT_NOTES.md", label: "Engagement notes", has: view.has_notes, hint: "scope, current state, open leads, ruled out" },
    { rel: "deliverables/report.md", label: "Report", has: view.has_report, hint: "what ships to the client" },
    { rel: "RESIDUAL_STATE.md", label: "Residual state", has: view.has_residual, hint: "what you created on the target" },
  ];

  return (
    <div className="flex h-full flex-col bg-card">
      <header className="flex items-center gap-2 border-b px-4 py-2.5">
        <FolderOpen className="size-4 text-muted-foreground" />
        <span className="text-[13px] font-semibold">Engagement</span>
        <span className="truncate font-mono text-[11.5px] text-muted-foreground">{view.dir}</span>
        <div className="flex-1" />
        <span className="text-[11px] text-muted-foreground">{view.files.length} files · {bytes(view.total_bytes)}</span>
        <button
          onClick={() => onReveal()}
          title="Open the engagement directory in the file manager"
          className="flex items-center gap-1.5 rounded-md border px-2 py-1 text-[11.5px] text-muted-foreground hover:bg-accent hover:text-foreground"
        >
          <ExternalLink className="size-3.5" />Open folder
        </button>
        <button onClick={onClose} className="rounded-md px-2 py-1 text-muted-foreground hover:bg-accent hover:text-foreground" title="Close">✕</button>
      </header>

      <nav className="flex gap-1 border-b px-3 py-1.5">
        {([["findings", ShieldAlert, `Findings${view.findings.length ? ` (${view.findings.length})` : ""}`],
           ["docs", ScrollText, "Report & notes"],
           ["artifacts", FileText, `Artifacts (${view.files.length})`]] as const).map(([k, Icon, label]) => (
          <button
            key={k}
            onClick={() => { setTab(k); setOpen(null); }}
            className={cn(
              "flex items-center gap-1.5 rounded-md px-2.5 py-1.5 text-[12.5px]",
              tab === k ? "bg-accent text-foreground" : "text-muted-foreground hover:bg-accent/60",
            )}
          >
            <Icon className="size-3.5" />{label}
          </button>
        ))}
      </nav>

      <div className="min-h-0 flex-1 overflow-auto">
        {tab === "findings" && (
          <div className="flex flex-col">
            {view.findings.length === 0 ? (
              <Empty>Nothing recorded yet. Findings appear as the agent runs <code>hacksor-findings add</code>.</Empty>
            ) : (
              <>
                <div className="flex flex-wrap gap-2 px-4 py-3">
                  {SEV.filter((s) => counts[s]).map((s) => (
                    <span key={s} className="text-[11.5px] text-muted-foreground">
                      <b style={{ color: sevColor[s] }}>{counts[s]}</b> {s}
                    </span>
                  ))}
                </div>
                <Separator />
                {view.findings.map((f) => (
                  <article key={f.id || f.title} className="border-b px-4 py-3">
                    <div className="flex items-start gap-3">
                      <div className="min-w-0 flex-1">
                        <div className="flex items-baseline gap-2">
                          {f.id && <span className="font-mono text-[11px] text-muted-foreground">{f.id}</span>}
                          <h3 className="truncate text-[13.5px] font-semibold">{f.title || "(untitled)"}</h3>
                        </div>
                        {f.url && <div className="truncate font-mono text-[11.5px] text-muted-foreground">{f.url}</div>}
                        {f.description && <p className="mt-1.5 text-[12.5px] leading-relaxed text-muted-foreground">{f.description}</p>}
                        {f.evidence && (
                          <pre className="mt-2 max-h-40 overflow-auto rounded-md border bg-background p-2 font-mono text-[11.5px] whitespace-pre-wrap">{f.evidence}</pre>
                        )}
                        {(f.owasp || f.status || f.cvss_vector) && (
                          <div className="mt-2 flex flex-wrap gap-3 text-[11px] text-muted-foreground">
                            {f.status && <span>status: {f.status}</span>}
                            {f.owasp && <span>{f.owasp}</span>}
                            {f.cvss_vector && <span className="font-mono">{f.cvss_vector}</span>}
                          </div>
                        )}
                      </div>
                      <SeverityPill severity={f.severity} cvss={f.cvss} />
                    </div>
                  </article>
                ))}
              </>
            )}
          </div>
        )}

        {tab === "docs" && (
          <div className="flex flex-col">
            {docs.map((d) => (
              <button
                key={d.rel}
                disabled={!d.has}
                onClick={() => setOpen(d.rel)}
                className={cn(
                  "flex flex-col items-start gap-0.5 border-b px-4 py-3 text-left",
                  d.has ? "hover:bg-accent/50" : "opacity-45",
                  open === d.rel && "bg-accent/60",
                )}
              >
                <span className="text-[13px] font-medium">{d.label}</span>
                <span className="text-[11.5px] text-muted-foreground">{d.has ? d.hint : "not created yet"}</span>
              </button>
            ))}
            <Preview rel={open} body={body} err={err} onReveal={onReveal} />
          </div>
        )}

        {tab === "artifacts" && (
          <div className="flex flex-col">
            <div className="relative px-3 py-2">
              <Search className="pointer-events-none absolute left-5 top-1/2 size-3.5 -translate-y-1/2 text-muted-foreground" />
              <Input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Filter by path…" className="h-8 pl-7 text-[12.5px]" />
            </div>
            {files.length === 0 ? (
              <Empty>No files match.</Empty>
            ) : (
              files.slice(0, 500).map((f) => (
                <button
                  key={f.rel}
                  onClick={() => setOpen(f.rel)}
                  className={cn(
                    "flex items-center gap-3 border-b px-4 py-2 text-left hover:bg-accent/50",
                    open === f.rel && "bg-accent/60",
                  )}
                >
                  <span className="min-w-0 flex-1 truncate font-mono text-[12px]">{f.rel}</span>
                  <span className="shrink-0 text-[11px] text-muted-foreground">{bytes(f.size)}</span>
                  <span className="shrink-0 text-[11px] text-muted-foreground">{ago(f.modified)}</span>
                </button>
              ))
            )}
            <Preview rel={open} body={body} err={err} onReveal={onReveal} />
          </div>
        )}
      </div>
    </div>
  );
}

function Empty({ children }: { children: React.ReactNode }) {
  return <div className="px-4 py-10 text-center text-[12.5px] text-muted-foreground">{children}</div>;
}

function Preview({ rel, body, err, onReveal }: { rel: string | null; body: string; err: string; onReveal: (rel?: string) => void }) {
  if (!rel) return null;
  // Notes and reports render as markdown; scripts and scan output get syntax
  // colour. Showing either as raw source is what this panel exists to avoid.
  const rendered = err || body === "Loading…" ? null : previewFile(rel, body);
  return (
    <div className="border-t">
      <div className="flex items-center gap-2 px-4 py-2">
        <span className="font-mono text-[11.5px] text-muted-foreground">{rel}</span>
        {rendered && <span className="text-[10.5px] uppercase tracking-wide text-muted-foreground opacity-60">{rendered.kind}</span>}
      </div>
      {err ? (
        <div className="flex items-center gap-3 px-4 pb-4">
          <span className="text-[12px] text-destructive">{err}</span>
          <button
            onClick={() => onReveal(rel)}
            className="shrink-0 rounded-md border px-2 py-1 text-[11.5px] text-muted-foreground hover:bg-accent hover:text-foreground"
          >
            Open in file manager
          </button>
        </div>
      ) : rendered ? (
        <div
          className={cn("pv-body max-h-[50vh] overflow-auto px-4 pb-4", rendered.kind === "markdown" && "bubble")}
          dangerouslySetInnerHTML={{ __html: rendered.html }}
        />
      ) : (
        <div className="px-4 pb-4 text-[12px] text-muted-foreground">{body}</div>
      )}
    </div>
  );
}
