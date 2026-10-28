//! Bundled specialist security skills (Strix) + local security-tool detection.

use std::path::{Path, PathBuf};

use include_dir::{include_dir, Dir};

// The `skills/` folder is embedded into the binary at build time.
static SKILLS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../skills");

/// Extract the bundled skills into `codex_home/skills` and return that path.
/// The agent reads them from disk with its own file/shell tools.
pub fn extract_skills(codex_home: &Path) -> PathBuf {
    let dst = codex_home.join("skills");
    let _ = std::fs::create_dir_all(&dst);
    let _ = SKILLS.extract(&dst);
    dst
}

/// How many skill files are bundled, counted from the embedded directory.
fn skill_count() -> usize {
    fn walk(d: &Dir) -> usize {
        d.files().filter(|f| f.path().extension().is_some_and(|e| e == "md")).count()
            + d.dirs().map(walk).sum::<usize>()
    }
    // CATALOG.md and README.md sit at the root and are not skills.
    walk(&SKILLS).saturating_sub(
        SKILLS.files().filter(|f| f.path().extension().is_some_and(|e| e == "md")).count(),
    )
}

/// Common security tools worth advertising when present locally.
const TOOLS: &[&str] = &[
    "nmap", "naabu", "httpx", "subfinder", "dnsrecon", "dnsenum", "whois", "gospider", "katana",
    "gobuster", "dirsearch", "ffuf", "arjun", "sqlmap", "nikto", "whatweb", "wafw00f", "wpscan",
    "wapiti", "nuclei", "testssl.sh", "smbclient", "smbmap", "enum4linux", "arp-scan", "hping3",
    "tcpdump", "hydra", "hashid", "cewl", "trufflehog", "jwt_tool", "interactsh-client", "rg",
    "proxychains4", "socat", "binwalk", "exiftool", "agent-browser", "curl", "python3", "go",
];

/// Best-effort PATH scan for installed security tools (no subprocess per tool).
pub fn detect_tools() -> Vec<&'static str> {
    let path = std::env::var("PATH").unwrap_or_default();
    let dirs: Vec<&str> = path.split(':').filter(|d| !d.is_empty()).collect();
    TOOLS
        .iter()
        .copied()
        .filter(|t| dirs.iter().any(|d| Path::new(d).join(t).exists()))
        .collect()
}

/// Build the dynamic environment-capabilities block appended to the persona:
/// where the skills + notes live, and which security tools are present locally.
pub fn env_note(skills_dir: &Path, notes_dir: &Path) -> String {
    let dir = skills_dir.to_string_lossy();
    // Counted, not written down: the figure was stated as 62 and stayed 62
    // through a refresh that brought the collection to 75.
    let count = skill_count();
    let notes = notes_dir.to_string_lossy();
    let tools = detect_tools();
    let host_line = if tools.is_empty() {
        "This host scan found no common security tools on PATH.".to_string()
    } else {
        format!("Tools detected on the host PATH: {}.", tools.join(", "))
    };
    // The host scan reflects only where Hacksor itself launched; in the bundled
    // Docker runtime the agent runs INSIDE the container with the full toolset,
    // which this scan does NOT see. So never conclude a tool is missing from this
    // line alone — verify with `command -v <tool>` in your own shell first.
    let tools_line = format!(
        "{host_line} NOTE: this scan is informational only and does NOT reflect the container you actually run in during Docker runtime mode — there the FULL specialist toolset (nmap, naabu, masscan, nuclei, httpx, subfinder, katana, ffuf, feroxbuster, sqlmap, dalfox, hydra, amass, hakrawler, gau, arjun, testssl.sh, interactsh, mitmproxy, agent-browser, …) is installed and on PATH. Always confirm a tool with `command -v <tool>` before deciding it is unavailable, and PREFER the specialist tool over hand-rolled curl.");
    format!(
        "<environment_capabilities>\n\
Specialist methodology skills ({count}, methodology only — no tools or scope) are on disk at {dir}. \
Browse them with `cat {dir}/CATALOG.md` or `ls -R {dir}`, then read the relevant one before testing that vuln class: `cat {dir}/<id>.md` (e.g. `cat {dir}/vulnerabilities/ssrf.md`).\n\
Persistent findings notes live in {notes} (Markdown files). Record important findings, methodology, and to-dos there so they persist across chats, and read them back when relevant.\n\
{tools_line}\n\
</environment_capabilities>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_skill_is_in_the_catalog() {
        // The catalog is the only index the agent reads before choosing a
        // skill, so a skill missing from it is a skill that never gets used.
        let catalog = SKILLS
            .get_file("CATALOG.md")
            .expect("skills/CATALOG.md is bundled")
            .contents_utf8()
            .expect("catalog is utf-8");
        let mut missing = Vec::new();
        fn walk(d: &Dir, catalog: &str, missing: &mut Vec<String>) {
            for f in d.files() {
                let p = f.path();
                if p.extension().is_some_and(|e| e == "md") && p.parent().is_some_and(|x| !x.as_os_str().is_empty()) {
                    let id = p.with_extension("");
                    let id = id.to_string_lossy().to_string();
                    if !catalog.contains(&format!("`{id}`")) {
                        missing.push(id);
                    }
                }
            }
            for sub in d.dirs() {
                walk(sub, catalog, missing);
            }
        }
        walk(&SKILLS, catalog, &mut missing);
        assert!(missing.is_empty(), "not listed in CATALOG.md: {missing:?}");
        assert!(skill_count() >= 75, "expected the full collection, got {}", skill_count());
    }
}
