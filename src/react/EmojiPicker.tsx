import { useEffect, useMemo, useRef, useState } from "react";
import { Input } from "@/components/ui/input";
import { cn } from "@/lib/utils";

type Compact = { hexcode: string; label: string; unicode: string; tags?: string[]; group?: number; order?: number };

/** emojibase's group ids, in the order people expect to scroll them. */
const GROUPS: { id: number; name: string }[] = [
  { id: 0, name: "Smileys & emotion" },
  { id: 1, name: "People & body" },
  { id: 3, name: "Animals & nature" },
  { id: 4, name: "Food & drink" },
  { id: 5, name: "Travel & places" },
  { id: 6, name: "Activities" },
  { id: 7, name: "Objects" },
  { id: 8, name: "Symbols" },
  { id: 9, name: "Flags" },
];

/** Shown before the dataset loads, and as the default row. */
export const QUICK = ["🎯", "🔍", "🧪", "🏢", "🌐", "🔐", "📦", "🚩", "⚠️", "📝"];

/**
 * Pick any emoji, by name.
 *
 * The category dialog offered ten fixed icons, which is fine until someone
 * wants the one that actually matches their engagement. The dataset is ~570 KB
 * so it loads on first open rather than at startup.
 *
 * Ranking matters more than it looks: a bare substring match puts "person
 * golfing" (tagged birdie) above the actual birds for a search of "bird", so
 * an exact label wins, then a label prefix, then a word in the label, then a
 * tag.
 */
export function EmojiPicker({ value, onPick }: { value: string; onPick: (e: string) => void }) {
  const [all, setAll] = useState<Compact[] | null>(null);
  const [q, setQ] = useState("");
  const [failed, setFailed] = useState(false);
  const boxRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let alive = true;
    import("emojibase-data/en/compact.json")
      .then((m) => { if (alive) setAll((m.default ?? m) as Compact[]); })
      .catch(() => { if (alive) setFailed(true); });
    return () => { alive = false; };
  }, []);

  const score = (e: Compact, ql: string): number => {
    const label = e.label.toLowerCase();
    if (label === ql) return 0;
    if (label.startsWith(ql)) return 1;
    if (label.split(/[\s-]+/).includes(ql)) return 2;
    if (label.includes(ql)) return 3;
    if (e.tags?.some((t) => t.toLowerCase() === ql)) return 4;
    if (e.tags?.some((t) => t.toLowerCase().includes(ql))) return 5;
    return 99;
  };

  const results = useMemo(() => {
    if (!all) return [];
    const ql = q.trim().toLowerCase();
    if (!ql) return [];
    return all
      .map((e) => ({ e, s: score(e, ql) }))
      .filter((x) => x.s < 99)
      .sort((a, b) => a.s - b.s || a.e.label.length - b.e.label.length)
      .slice(0, 180)
      .map((x) => x.e);
  }, [all, q]);

  const grouped = useMemo(() => {
    if (!all) return [];
    return GROUPS.map((g) => ({
      ...g,
      items: all.filter((e) => e.group === g.id).slice(0, 120),
    })).filter((g) => g.items.length);
  }, [all]);

  const Cell = ({ e }: { e: string }) => (
    <button
      type="button"
      onClick={() => onPick(e)}
      title={e}
      className={cn(
        "grid size-8 place-content-center rounded-md text-[18px] leading-none hover:bg-accent",
        value === e && "bg-accent ring-1 ring-foreground",
      )}
    >
      {e}
    </button>
  );

  return (
    <div className="flex flex-col gap-2">
      <Input
        value={q}
        onChange={(ev) => setQ(ev.target.value)}
        placeholder={all ? "Search emoji — try bird, lock, cloud…" : "Loading emoji…"}
        className="h-8 text-[12.5px]"
      />
      <div ref={boxRef} className="max-h-52 overflow-y-auto rounded-lg border p-1.5">
        {failed ? (
          <div className="flex flex-wrap gap-1">{QUICK.map((e) => <Cell key={e} e={e} />)}</div>
        ) : !all ? (
          <div className="flex flex-wrap gap-1 opacity-60">{QUICK.map((e) => <Cell key={e} e={e} />)}</div>
        ) : q.trim() ? (
          results.length ? (
            <div className="flex flex-wrap gap-1">{results.map((e) => <Cell key={e.hexcode} e={e.unicode} />)}</div>
          ) : (
            <div className="px-2 py-6 text-center text-[12px] text-muted-foreground">No emoji matches “{q.trim()}”.</div>
          )
        ) : (
          grouped.map((g) => (
            <section key={g.id} className="mb-1.5">
              <h4 className="px-1 py-1 text-[10.5px] uppercase tracking-wide text-muted-foreground">{g.name}</h4>
              <div className="flex flex-wrap gap-1">{g.items.map((e) => <Cell key={e.hexcode} e={e.unicode} />)}</div>
            </section>
          ))
        )}
      </div>
    </div>
  );
}
