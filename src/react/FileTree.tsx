import { useMemo, useState } from "react";
import { ChevronRight, Folder, FolderOpen, File } from "lucide-react";
import { cn } from "@/lib/utils";

export type Entry = { rel: string; size: number; modified: number };

type Dir = {
  name: string;
  path: string;
  dirs: Map<string, Dir>;
  files: Entry[];
  /** Totals for everything beneath, so a collapsed folder still says how much is in it. */
  count: number;
  bytes: number;
  newest: number;
};

const mkdir = (name: string, path: string): Dir => ({
  name, path, dirs: new Map(), files: [], count: 0, bytes: 0, newest: 0,
});

/** Build the directory tree from flat `a/b/c.txt` paths. */
function build(entries: Entry[]): Dir {
  const root = mkdir("", "");
  for (const e of entries) {
    const parts = e.rel.split("/");
    const file = parts.pop()!;
    let cur = root;
    let path = "";
    for (const p of parts) {
      path = path ? `${path}/${p}` : p;
      let next = cur.dirs.get(p);
      if (!next) { next = mkdir(p, path); cur.dirs.set(p, next); }
      cur = next;
    }
    cur.files.push({ ...e, rel: e.rel });
    void file;
  }
  // Roll totals up once, rather than walking the subtree on every render.
  const roll = (d: Dir) => {
    for (const c of d.dirs.values()) roll(c);
    d.count = d.files.length;
    d.bytes = d.files.reduce((n, f) => n + f.size, 0);
    d.newest = d.files.reduce((n, f) => Math.max(n, f.modified), 0);
    for (const c of d.dirs.values()) {
      d.count += c.count; d.bytes += c.bytes; d.newest = Math.max(d.newest, c.newest);
    }
  };
  roll(root);
  return root;
}

/**
 * Collapse runs of single-child directories into one row.
 *
 * An engagement is full of `artifacts/recon/nmap/...`, and showing three rows
 * that each contain only the next is three clicks to reach anything. They
 * render as `artifacts/recon/nmap`.
 */
function chain(d: Dir): { label: string; dir: Dir } {
  let label = d.name;
  let cur = d;
  while (cur.files.length === 0 && cur.dirs.size === 1) {
    const only = [...cur.dirs.values()][0];
    label += "/" + only.name;
    cur = only;
  }
  return { label, dir: cur };
}

export function FileTree({
  entries, open, onOpen, bytes, ago, filtering,
}: {
  entries: Entry[];
  open: string | null;
  onOpen: (rel: string) => void;
  bytes: (n: number) => string;
  ago: (t: number) => string;
  /** While filtering, everything is expanded: hiding matches behind a closed folder defeats the search. */
  filtering: boolean;
}) {
  const root = useMemo(() => build(entries), [entries]);
  // Only what the viewer has explicitly toggled. Everything else follows the
  // default below, so the tree stays compact without freezing a decision the
  // viewer never made.
  const [toggled, setToggled] = useState<Map<string, boolean>>(() => new Map());
  const toggle = (p: string, isOpen: boolean) =>
    setToggled((m) => new Map(m).set(p, !isOpen));

  // Top level open, everything under it closed. Fully expanded, a real
  // engagement opens as a wall of a few hundred rows, which is the thing that
  // made the flat list hard to read in the first place; fully collapsed hides
  // the shape. One level shows the workstreams and their totals.
  const defaultOpen = (depth: number) => depth === 0;

  const Row = ({ d, depth }: { d: Dir; depth: number }) => {
    const { label, dir } = chain(d);
    const isOpen = filtering || (toggled.get(dir.path) ?? defaultOpen(depth));
    return (
      <>
        <button
          onClick={() => toggle(dir.path, isOpen)}
          className="flex w-full items-center gap-2 border-b px-4 py-1.5 text-left hover:bg-accent/50"
          style={{ paddingLeft: 16 + depth * 14 }}
        >
          <ChevronRight className={cn("size-3.5 shrink-0 text-muted-foreground transition-transform", isOpen && "rotate-90")} />
          {isOpen ? <FolderOpen className="size-4 shrink-0 text-muted-foreground" /> : <Folder className="size-4 shrink-0 text-muted-foreground" />}
          <span className="min-w-0 flex-1 truncate font-mono text-[12px]">{label}</span>
          <span className="shrink-0 text-[11px] text-muted-foreground">{dir.count} file{dir.count === 1 ? "" : "s"}</span>
          <span className="shrink-0 text-[11px] text-muted-foreground">{bytes(dir.bytes)}</span>
          <span className="shrink-0 text-[11px] text-muted-foreground">{ago(dir.newest)}</span>
        </button>
        {isOpen && <Children d={dir} depth={depth + 1} />}
      </>
    );
  };

  const Children = ({ d, depth }: { d: Dir; depth: number }) => (
    <>
      {[...d.dirs.values()]
        .sort((a, b) => b.newest - a.newest || a.name.localeCompare(b.name))
        .map((c) => <Row key={c.path} d={c} depth={depth} />)}
      {d.files
        .slice()
        .sort((a, b) => b.modified - a.modified || a.rel.localeCompare(b.rel))
        .map((f) => (
          <button
            key={f.rel}
            onClick={() => onOpen(f.rel)}
            className={cn(
              "flex w-full items-center gap-2 border-b px-4 py-1.5 text-left hover:bg-accent/50",
              open === f.rel && "bg-accent/60",
            )}
            style={{ paddingLeft: 16 + depth * 14 }}
          >
            <File className="size-4 shrink-0 text-muted-foreground opacity-70" />
            <span className="min-w-0 flex-1 truncate font-mono text-[12px]">{f.rel.split("/").pop()}</span>
            <span className="shrink-0 text-[11px] text-muted-foreground">{bytes(f.size)}</span>
            <span className="shrink-0 text-[11px] text-muted-foreground">{ago(f.modified)}</span>
          </button>
        ))}
    </>
  );

  return <Children d={root} depth={0} />;
}
