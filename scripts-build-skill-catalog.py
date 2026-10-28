#!/usr/bin/env python3
"""Regenerate skills/CATALOG.md and skills/CATALOG.txt from the skill files.

The catalog is what the agent reads first to decide which skill to open, so it
has to list every skill that exists. It was written by hand, which meant a
refresh from upstream silently left new skills undiscoverable: present on disk,
absent from the index, never read.

    python3 scripts-build-skill-catalog.py
"""
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).parent / "skills"
# A translation note, not a patch.
#
# The skills are vendored from upstream Strix verbatim so a refresh stays a
# copy. They therefore name Strix's own tools, which do not exist here. Rather
# than edit 75 files and re-edit them on every refresh, the mapping lives in
# the header of the first thing the agent reads.
HEAD = (
    "# Specialist security skills catalog\n\n"
    "Methodology only (no tools/scope). Read a skill file before testing that "
    "class: `cat <SKILLS_DIR>/<id>.md`\n\n"
    "## Reading these in Hacksor\n\n"
    "These are vendored from upstream Strix unchanged, so they occasionally "
    "name tools from that harness. Translate as you read:\n\n"
    "- `exec_command(...)` — just run it in your shell.\n"
    "- `create_finding(...)`, `create_dependency_report(...)` — record it with "
    "`hacksor-findings add` instead, and regenerate the report from that store.\n"
    "- `create_agent(..., skills=...)` — you read skills yourself with `cat`; "
    "to fan work out, use `hacksor-subagent`.\n"
    "- Caido / `caido_api` — not installed. The intercepting proxy here is "
    "`hacksor-proxy`.\n"
    "- `hurl` — not installed; `tooling/hurl` is still useful as method, but "
    "reach for curl or python.\n\n"
    "Anything else a skill assumes: confirm with `command -v <tool>` before "
    "concluding it is missing, and prefer the specialist tool over hand-rolled "
    "curl when it is there.\n"
)


def front_matter(text: str) -> dict:
    m = re.match(r"^---\n(.*?)\n---", text, re.S)
    if not m:
        return {}
    out = {}
    for line in m.group(1).splitlines():
        if ":" in line:
            k, v = line.split(":", 1)
            out[k.strip()] = v.strip().strip('"').strip("'")
    return out


def main() -> int:
    if not ROOT.is_dir():
        print(f"no skills directory at {ROOT}", file=sys.stderr)
        return 1
    out = [HEAD]
    total = 0
    for cat in sorted(p for p in ROOT.iterdir() if p.is_dir()):
        rows = []
        for f in sorted(cat.glob("*.md")):
            fm = front_matter(f.read_text(encoding="utf-8"))
            desc = fm.get("description") or ""
            # The id is the FILENAME, never the frontmatter name: upstream
            # writes `name: sql-injection` for sql_injection.md, and the
            # catalog's whole purpose is to give a path that `cat` can open.
            # 32 of 75 diverge, so taking the name breaks a third of them.
            rows.append(f"- `{cat.name}/{f.stem}` — {desc}")
        if not rows:
            continue
        total += len(rows)
        out.append(f"\n## {cat.name}\n" + "\n".join(rows) + "\n")
    body = "".join(out)
    (ROOT / "CATALOG.md").write_text(body, encoding="utf-8")
    # The .txt is the same content: some tools read one, some the other, and
    # letting them drift apart is how the agent gets two different answers.
    (ROOT / "CATALOG.txt").write_text(body, encoding="utf-8")
    print(f"catalog rebuilt: {total} skills")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
