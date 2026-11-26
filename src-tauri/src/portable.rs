//! Portable sessions: export one chat to a `.hacksor` file, import it on
//! another machine, and carry on where it stopped.
//!
//! The point is NOT to hand a new agent a summary to read. The model API is
//! stateless — every turn resends the whole conversation — so a session is
//! entirely a local file, and codex proves it: given a rollout it never wrote,
//! under an id it has never seen, `thread/resume` returns the thread with its
//! model and working directory intact. Copy that file and the work continues
//! with the same history, the same tool calls and the same results.
//!
//! What a bundle therefore holds:
//!   - the rollout, which IS the conversation;
//!   - the engagement directory and the memory lines the work produced;
//!   - derived analysis, which is the expensive part to recompute;
//!   - hashes of the target files, so the other machine can prove it has the
//!     same ones rather than assume it.
//!
//! What it never holds: API keys or settings.
//!
//! The target files themselves are left out by default. A conversation about a
//! 2 GB binary is useless without the binary, but shipping someone's target
//! inside a transcript should be a decision, not a default.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

/// `HACKSOR1` then one byte: 0 plain, 1 encrypted. Readable with `head -c 9`,
/// so a bundle can be identified without the app.
const MAGIC: &[u8; 8] = b"HACKSOR1";
const FLAG_PLAIN: u8 = 0;
const FLAG_ENCRYPTED: u8 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
/// Deliberately slow: the passphrase guards a transcript that can contain
/// harvested credentials, and the bundle may travel over channels the user does
/// not control.
const PBKDF2_ITERS: u32 = 600_000;

/// Bumped when the layout changes in a way an older import cannot read.
pub const SCHEMA: u32 = 1;

/// Caps on the derived analysis carried along. Generous enough for decompiler
/// output and reports, small enough that a bundle stays sendable.
const MAX_ARTIFACT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ARTIFACTS_TOTAL: u64 = 192 * 1024 * 1024;
/// Hashing is cheap per file but a working directory can hold a tree of
/// thousands; past this the manifest stops being useful anyway.
const MAX_HASHED_FILES: usize = 20_000;
/// Files above this are recorded by size and path only — hashing a multi-GB
/// disk image to prove identity is not worth the wait.
const MAX_HASH_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// What the bundle says about itself. Written as `manifest.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub app_version: String,
    pub exported_at: String,
    pub source_os: String,
    pub thread_id: String,
    pub title: String,
    /// The model the conversation ran on, so the other machine can continue on
    /// the same one rather than silently switching.
    pub model: Option<String>,
    pub provider: Option<String>,
    pub context_window: Option<u64>,
    /// The working directory on the machine that exported it. Almost never the
    /// path on the machine that imports it — see `mapping_note`.
    pub cwd: Option<String>,
    pub engagement: Option<String>,
    pub counts: Counts,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Counts {
    pub rollout_lines: usize,
    pub artifacts: usize,
    pub workspace_files: usize,
    pub memory_lines: usize,
}

/// One target file, identified rather than carried.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceEntry {
    pub path: String,
    pub size: u64,
    /// `None` when the file was too large to hash.
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportSummary {
    pub path: String,
    pub bytes: u64,
    pub encrypted: bool,
    pub manifest: Manifest,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportSummary {
    pub thread_id: String,
    /// What to call the chat here. Prefixed, because an imported session
    /// describes work done somewhere else: the history may name paths, hosts
    /// and files that this machine does not have, and that is worth knowing
    /// from the sidebar rather than after reading a confusing answer.
    pub title: String,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub source_cwd: Option<String>,
    pub local_cwd: String,
    pub engagement: Option<String>,
    /// Target files the bundle described that are missing locally.
    pub missing_files: Vec<String>,
    /// Target files present locally whose contents differ from the source.
    pub changed_files: Vec<String>,
    pub restored_artifacts: usize,
}

// ---------------------------------------------------------------- sealing

fn derive_key(passphrase: &str, salt: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];
    ring::pbkdf2::derive(
        ring::pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(PBKDF2_ITERS).expect("non-zero"),
        salt,
        passphrase.as_bytes(),
        &mut key,
    );
    key
}

/// Wrap the archive in the envelope that is written to disk.
pub fn seal(archive: &[u8], passphrase: Option<&str>) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(archive.len() + 64);
    out.extend_from_slice(MAGIC);
    let Some(pass) = passphrase.filter(|p| !p.is_empty()) else {
        out.push(FLAG_PLAIN);
        out.extend_from_slice(archive);
        return Ok(out);
    };

    use ring::rand::SecureRandom;
    let rng = ring::rand::SystemRandom::new();
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    rng.fill(&mut salt).map_err(|_| anyhow!("no secure randomness available"))?;
    rng.fill(&mut nonce).map_err(|_| anyhow!("no secure randomness available"))?;

    let key = derive_key(pass, &salt);
    let unbound = ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &key)
        .map_err(|_| anyhow!("could not build the encryption key"))?;
    let sealing = ring::aead::LessSafeKey::new(unbound);
    let mut body = archive.to_vec();
    sealing
        .seal_in_place_append_tag(
            ring::aead::Nonce::assume_unique_for_key(nonce),
            ring::aead::Aad::empty(),
            &mut body,
        )
        .map_err(|_| anyhow!("could not encrypt the bundle"))?;

    out.push(FLAG_ENCRYPTED);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&body);
    Ok(out)
}

/// Unwrap what `seal` wrote, or say precisely why it could not be unwrapped.
pub fn unseal(file: &[u8], passphrase: Option<&str>) -> Result<Vec<u8>> {
    if file.len() < MAGIC.len() + 1 || &file[..MAGIC.len()] != MAGIC {
        bail!("not a Hacksor session bundle");
    }
    let flag = file[MAGIC.len()];
    let rest = &file[MAGIC.len() + 1..];
    match flag {
        FLAG_PLAIN => Ok(rest.to_vec()),
        FLAG_ENCRYPTED => {
            let Some(pass) = passphrase.filter(|p| !p.is_empty()) else {
                bail!("this bundle is encrypted — a passphrase is needed to import it");
            };
            if rest.len() < SALT_LEN + NONCE_LEN + 16 {
                bail!("the bundle is truncated or corrupt");
            }
            let salt = &rest[..SALT_LEN];
            let mut nonce = [0u8; NONCE_LEN];
            nonce.copy_from_slice(&rest[SALT_LEN..SALT_LEN + NONCE_LEN]);
            let key = derive_key(pass, salt);
            let unbound = ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &key)
                .map_err(|_| anyhow!("could not build the decryption key"))?;
            let opening = ring::aead::LessSafeKey::new(unbound);
            let mut body = rest[SALT_LEN + NONCE_LEN..].to_vec();
            let plain = opening
                .open_in_place(
                    ring::aead::Nonce::assume_unique_for_key(nonce),
                    ring::aead::Aad::empty(),
                    &mut body,
                )
                // AES-GCM cannot tell a wrong key from damaged bytes, and
                // guessing wrong in the message would send the user hunting the
                // wrong problem.
                .map_err(|_| anyhow!("wrong passphrase, or the bundle has been altered"))?;
            Ok(plain.to_vec())
        }
        other => bail!("unknown bundle format (flag {other})"),
    }
}

// ---------------------------------------------------------------- helpers

fn sha256_file(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return None,
        }
    }
    Some(format!("{:x}", hasher.finalize()))
}

/// Is this file derived analysis worth carrying, rather than a target binary?
///
/// Extension-based on purpose: the question is "would recomputing this cost the
/// user time", and the answer tracks text-shaped output — decompiler listings,
/// notes, reports, extracted strings — not what the file magic says.
fn is_artifact(path: &Path, size: u64) -> bool {
    if size > MAX_ARTIFACT_BYTES {
        return false;
    }
    const KEEP: &[&str] = &[
        "md", "txt", "json", "jsonl", "csv", "log", "c", "h", "cpp", "hpp", "rs", "py", "js", "ts",
        "yaml", "yml", "toml", "ini", "xml", "html", "sql", "sh", "ps1", "asm", "s", "lst", "dis",
        "decomp", "idc", "patch", "diff", "nmap", "gnmap", "sarif",
    ];
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| KEEP.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Walk a directory, skipping the places that hold machine-local noise rather
/// than work: version control internals, dependency trees, build output.
fn walk_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    const SKIP: &[&str] = &[".git", "node_modules", "target", "__pycache__", ".venv", "venv", ".cache"];
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_symlink() {
                continue;
            }
            if meta.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if SKIP.contains(&name.as_ref()) {
                    continue;
                }
                stack.push(path);
            } else if meta.is_file() {
                out.push(path);
                if out.len() >= limit {
                    return out;
                }
            }
        }
    }
    out
}

/// The thread's first record describes the session; read what we need from it.
#[derive(Default)]
pub struct SessionMeta {
    pub cwd: Option<String>,
    pub provider: Option<String>,
    pub context_window: Option<u64>,
}

pub fn session_meta(rollout: &str) -> SessionMeta {
    let Some(first) = rollout.lines().find(|l| !l.trim().is_empty()) else {
        return SessionMeta::default();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(first) else {
        return SessionMeta::default();
    };
    let p = v.get("payload").unwrap_or(&v);
    SessionMeta {
        cwd: p.get("cwd").and_then(|x| x.as_str()).map(String::from),
        provider: p.get("model_provider").and_then(|x| x.as_str()).map(String::from),
        context_window: p.get("context_window").and_then(|x| x.as_u64()),
    }
}

/// The model the conversation actually ran on.
///
/// `session_meta` records the provider but not always the model, and a thread
/// can change model partway, so take the LAST one named: that is what the user
/// was using when they exported, and what "continue on the same model" means.
pub fn model_from_rollout(rollout: &str) -> Option<String> {
    let mut found = None;
    for line in rollout.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let p = v.get("payload").unwrap_or(&v);
        if let Some(m) = p.get("model").and_then(|x| x.as_str()) {
            if !m.is_empty() {
                found = Some(m.to_string());
            }
        }
    }
    found
}

/// Give the thread a new identity, so importing twice — or importing a thread
/// that began on this machine — cannot overwrite an existing conversation.
pub fn reidentify(rollout: &str, old_id: &str, new_id: &str) -> String {
    if old_id.is_empty() {
        return rollout.to_string();
    }
    rollout.replace(old_id, new_id)
}

/// The note prepended to an imported thread.
///
/// The history is full of the other machine's absolute paths, and the model
/// will otherwise take them at face value. Rewriting that history would be
/// worse: it would make the transcript claim things that never happened. So
/// the truth is told once, in a line the model reads before continuing.
pub fn mapping_note(m: &Manifest, local_cwd: &str, missing: &[String], changed: &[String]) -> String {
    let mut s = String::new();
    s.push_str("[imported session] This conversation was exported from another machine (");
    s.push_str(&m.source_os);
    s.push_str(") and imported here. The history below is the real conversation, not a summary.\n");
    if let Some(src) = &m.cwd {
        if src != local_cwd {
            s.push_str(&format!(
                "Paths have moved: what the history calls {src} is {local_cwd} on this machine. Translate any path you read from the history before using it.\n"
            ));
        }
    }
    if !missing.is_empty() {
        s.push_str(&format!(
            "{} file(s) the earlier work referred to are NOT on this machine, including: {}. Do not assume their contents from the history — say so if they are needed.\n",
            missing.len(),
            missing.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if !changed.is_empty() {
        s.push_str(&format!(
            "{} file(s) exist here but differ from the ones analysed earlier, including: {}. Re-read those before relying on earlier conclusions about them.\n",
            changed.len(),
            changed.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    s.push_str("Analysis carried over from the other machine is under .hacksor-imported/ in the working directory.\n");
    s
}

/// The highest `ordinal` in a rollout.
///
/// Records are paginated and codex refuses to resume a thread whose final
/// record has no ordinal ("final paginated rollout record ... is missing an
/// ordinal"), so anything appended has to continue the sequence rather than
/// invent one. Found the hard way, by appending a record without it.
pub fn last_ordinal(rollout: &str) -> u64 {
    rollout
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|v| v.get("ordinal").and_then(|o| o.as_u64()))
        .unwrap_or(0)
}

/// Build a rollout record carrying the note, shaped like the user messages
/// already in the file so codex reads it as part of the conversation.
pub fn note_record(note: &str, timestamp: &str, ordinal: u64) -> String {
    serde_json::json!({
        "timestamp": timestamp,
        "type": "response_item",
        "ordinal": ordinal,
        "payload": {
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": note }]
        }
    })
    .to_string()
}

// ---------------------------------------------------------------- export

/// Collect a session into a bundle. `rollout_path` is the thread's own file.
pub fn build_bundle(
    rollout_path: &Path,
    manifest_base: Manifest,
    engagement_dir: Option<&Path>,
    memory_files: &[PathBuf],
    workspace: Option<&Path>,
) -> Result<(Vec<u8>, Manifest)> {
    let rollout = std::fs::read_to_string(rollout_path)
        .with_context(|| format!("reading the conversation at {}", rollout_path.display()))?;
    let mut manifest = manifest_base;
    manifest.schema = SCHEMA;
    manifest.counts.rollout_lines = rollout.lines().filter(|l| !l.trim().is_empty()).count();

    let mut tar = tar::Builder::new(Vec::new());

    let append = |tar: &mut tar::Builder<Vec<u8>>, name: &str, data: &[u8]| -> Result<()> {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        tar.append_data(&mut header, name, data)?;
        Ok(())
    };

    append(&mut tar, "thread.jsonl", rollout.as_bytes())?;

    // The engagement directory: notes and findings, which are small and are the
    // human-readable half of the work.
    if let Some(dir) = engagement_dir.filter(|d| d.is_dir()) {
        for file in walk_files(dir, 2_000) {
            let Ok(meta) = file.metadata() else { continue };
            if meta.len() > MAX_ARTIFACT_BYTES {
                continue;
            }
            let rel = file.strip_prefix(dir).unwrap_or(&file);
            let name = format!("engagement/{}", rel.to_string_lossy().replace('\\', "/"));
            if let Ok(bytes) = std::fs::read(&file) {
                append(&mut tar, &name, &bytes)?;
            }
        }
    }

    for mem in memory_files {
        if let Ok(bytes) = std::fs::read(mem) {
            manifest.counts.memory_lines += bytes.iter().filter(|b| **b == b'\n').count();
            let name = format!(
                "memory/{}",
                mem.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
            );
            append(&mut tar, &name, &bytes)?;
        }
    }

    // Derived analysis, plus an identity record for every target file.
    if let Some(root) = workspace.filter(|w| w.is_dir()) {
        let mut entries: Vec<WorkspaceEntry> = Vec::new();
        let mut artifact_total: u64 = 0;
        for file in walk_files(root, MAX_HASHED_FILES) {
            let Ok(meta) = file.metadata() else { continue };
            let size = meta.len();
            let rel = file.strip_prefix(root).unwrap_or(&file);
            let rel_name = rel.to_string_lossy().replace('\\', "/");
            entries.push(WorkspaceEntry {
                path: rel_name.clone(),
                size,
                sha256: if size <= MAX_HASH_BYTES { sha256_file(&file) } else { None },
            });
            if is_artifact(&file, size) && artifact_total + size <= MAX_ARTIFACTS_TOTAL {
                if let Ok(bytes) = std::fs::read(&file) {
                    artifact_total += size;
                    manifest.counts.artifacts += 1;
                    append(&mut tar, &format!("artifacts/{rel_name}"), &bytes)?;
                }
            }
        }
        manifest.counts.workspace_files = entries.len();
        append(&mut tar, "workspace.json", serde_json::to_string_pretty(&entries)?.as_bytes())?;
    }

    append(&mut tar, "manifest.json", serde_json::to_string_pretty(&manifest)?.as_bytes())?;

    let tar_bytes = tar.into_inner().context("finishing the archive")?;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut gz, &tar_bytes)?;
    Ok((gz.finish()?, manifest))
}

// ---------------------------------------------------------------- import

/// One file unpacked from a bundle.
#[derive(Debug)]
pub struct Unpacked {
    pub manifest: Manifest,
    pub rollout: String,
    pub workspace: Vec<WorkspaceEntry>,
    /// name (without its `artifacts/` prefix) → contents
    pub artifacts: Vec<(String, Vec<u8>)>,
    pub engagement: Vec<(String, Vec<u8>)>,
    pub memory: Vec<(String, Vec<u8>)>,
}

pub fn unpack(archive: &[u8]) -> Result<Unpacked> {
    let gz = flate2::read::GzDecoder::new(archive);
    let mut ar = tar::Archive::new(gz);
    let mut manifest: Option<Manifest> = None;
    let mut rollout = String::new();
    let mut workspace = Vec::new();
    let mut artifacts = Vec::new();
    let mut engagement = Vec::new();
    let mut memory = Vec::new();

    for entry in ar.entries().context("reading the bundle")? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;
        // A tar can name anything, including `../../etc/passwd`. Nothing here is
        // written by its own name, but reject the attempt rather than carry it.
        if path.contains("..") {
            bail!("the bundle contains an unsafe path: {path}");
        }
        match path.as_str() {
            "manifest.json" => manifest = Some(serde_json::from_slice(&buf)?),
            "thread.jsonl" => rollout = String::from_utf8_lossy(&buf).to_string(),
            "workspace.json" => workspace = serde_json::from_slice(&buf).unwrap_or_default(),
            p if p.starts_with("artifacts/") => {
                artifacts.push((p.trim_start_matches("artifacts/").to_string(), buf))
            }
            p if p.starts_with("engagement/") => {
                engagement.push((p.trim_start_matches("engagement/").to_string(), buf))
            }
            p if p.starts_with("memory/") => {
                memory.push((p.trim_start_matches("memory/").to_string(), buf))
            }
            _ => {}
        }
    }

    let manifest = manifest.ok_or_else(|| anyhow!("the bundle has no manifest"))?;
    if manifest.schema > SCHEMA {
        bail!(
            "this bundle was written by a newer Hacksor (format {}, this one reads {SCHEMA}) — update and try again",
            manifest.schema
        );
    }
    if rollout.trim().is_empty() {
        bail!("the bundle has no conversation in it");
    }
    Ok(Unpacked { manifest, rollout, workspace, artifacts, engagement, memory })
}

/// Compare what the bundle described against what is on this machine.
pub fn check_workspace(entries: &[WorkspaceEntry], local_root: &Path) -> (Vec<String>, Vec<String>) {
    let mut missing = Vec::new();
    let mut changed = Vec::new();
    for e in entries {
        let local = local_root.join(&e.path);
        if !local.is_file() {
            missing.push(e.path.clone());
            continue;
        }
        let Some(expected) = &e.sha256 else { continue };
        if sha256_file(&local).as_deref() != Some(expected.as_str()) {
            changed.push(e.path.clone());
        }
    }
    (missing, changed)
}

/// Install an unpacked bundle on this machine: restore what it carried, give
/// the conversation a new identity, tell it where it woke up, and write it
/// where codex looks for threads.
///
/// Nothing here overwrites the user's own work. Artifacts land in their own
/// directory, an engagement that would collide is renamed, memory is appended
/// rather than replaced, and the thread always takes a fresh id.
pub fn install(
    bundle: &Unpacked,
    codex_home: &Path,
    local_cwd: &Path,
    hacksor_home: &Path,
) -> Result<ImportSummary> {
    let (missing, changed) = check_workspace(&bundle.workspace, local_cwd);

    let artifact_root = local_cwd.join(".hacksor-imported");
    let mut restored = 0usize;
    for (name, body) in &bundle.artifacts {
        let dest = artifact_root.join(name);
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&dest, body).is_ok() {
            restored += 1;
        }
    }

    let mut engagement_name = None;
    if let Some(name) = &bundle.manifest.engagement {
        let mut dir = hacksor_home.join("engagements").join(name);
        if dir.exists() {
            dir = hacksor_home.join("engagements").join(format!("{name}-imported"));
        }
        for (rel, body) in &bundle.engagement {
            let dest = dir.join(rel);
            if let Some(parent) = dest.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(dest, body);
        }
        engagement_name = dir.file_name().map(|n| n.to_string_lossy().to_string());
    }

    for (name, body) in &bundle.memory {
        let dest = hacksor_home.join(name);
        let existing = std::fs::read_to_string(&dest).unwrap_or_default();
        let mut out = existing.clone();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        for line in String::from_utf8_lossy(body).lines() {
            if !line.trim().is_empty() && !existing.contains(line) {
                out.push_str(line);
                out.push('\n');
            }
        }
        let _ = std::fs::create_dir_all(hacksor_home);
        let _ = std::fs::write(dest, out);
    }

    let new_id = new_thread_id();
    let mut rollout = reidentify(&bundle.rollout, &bundle.manifest.thread_id, &new_id);
    if !rollout.ends_with('\n') {
        rollout.push('\n');
    }
    let note = mapping_note(&bundle.manifest, &local_cwd.to_string_lossy(), &missing, &changed);
    let now = chrono::Utc::now();
    let ordinal = last_ordinal(&rollout) + 1;
    rollout.push_str(&note_record(&note, &now.to_rfc3339(), ordinal));
    rollout.push('\n');

    // codex resolves a thread by finding its rollout under this tree, so
    // writing the file here is the whole of "installing" it.
    let dir = codex_home
        .join("sessions")
        .join(now.format("%Y").to_string())
        .join(now.format("%m").to_string())
        .join(now.format("%d").to_string());
    std::fs::create_dir_all(&dir).context("creating the sessions directory")?;
    let name = format!("rollout-{}-{new_id}.jsonl", now.format("%Y-%m-%dT%H-%M-%S"));
    std::fs::write(dir.join(&name), rollout).context("writing the imported conversation")?;

    Ok(ImportSummary {
        thread_id: new_id,
        title: imported_title(&bundle.manifest.title),
        model: bundle.manifest.model.clone(),
        provider: bundle.manifest.provider.clone(),
        source_cwd: bundle.manifest.cwd.clone(),
        local_cwd: local_cwd.to_string_lossy().into_owned(),
        engagement: engagement_name,
        missing_files: missing,
        changed_files: changed,
        restored_artifacts: restored,
    })
}

/// The sidebar name for an imported chat.
pub fn imported_title(original: &str) -> String {
    let t = original.trim();
    if t.is_empty() {
        return "Imported session".into();
    }
    // Don't stack the prefix when a bundle is passed on a second time.
    if t.starts_with("Imported:") {
        return t.to_string();
    }
    format!("Imported: {t}")
}

/// A fresh thread id, so an import never lands on an existing conversation.
fn new_thread_id() -> String {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 16];
    if ring::rand::SystemRandom::new().fill(&mut b).is_err() {
        let n = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default() as u128;
        b.copy_from_slice(&n.to_be_bytes());
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            schema: SCHEMA,
            app_version: "test".into(),
            exported_at: "2026-11-26T00:00:00Z".into(),
            source_os: "macos".into(),
            thread_id: "old-id".into(),
            title: "A session".into(),
            model: Some("anthropic/claude-opus-5".into()),
            provider: Some("opencodex".into()),
            context_window: Some(1_000_000),
            cwd: Some("/Users/a/work".into()),
            engagement: None,
            counts: Counts::default(),
        }
    }

    #[test]
    fn a_sealed_bundle_comes_back_byte_for_byte() {
        let body = b"the whole conversation".to_vec();
        let sealed = seal(&body, Some("correct horse")).unwrap();
        assert_eq!(unseal(&sealed, Some("correct horse")).unwrap(), body);
    }

    #[test]
    fn the_wrong_passphrase_is_refused_rather_than_half_decrypted() {
        let sealed = seal(b"secret findings", Some("right")).unwrap();
        let err = unseal(&sealed, Some("wrong")).unwrap_err().to_string();
        assert!(err.contains("wrong passphrase"), "{err}");
    }

    #[test]
    fn an_encrypted_bundle_says_so_instead_of_failing_obscurely() {
        let sealed = seal(b"x", Some("pw")).unwrap();
        let err = unseal(&sealed, None).unwrap_err().to_string();
        assert!(err.contains("passphrase is needed"), "{err}");
    }

    #[test]
    fn plaintext_does_not_appear_in_an_encrypted_bundle() {
        // The reason the passphrase exists: transcripts hold harvested
        // credentials, so the bytes on disk must not contain them.
        let secret = "PASSWORD=hunter2-from-the-target";
        let sealed = seal(secret.as_bytes(), Some("pw")).unwrap();
        let haystack = String::from_utf8_lossy(&sealed);
        assert!(!haystack.contains("hunter2"), "secret survived into the file");
    }

    #[test]
    fn a_foreign_file_is_rejected_with_a_plain_message() {
        let err = unseal(b"PK\x03\x04 not ours", Some("pw")).unwrap_err().to_string();
        assert!(err.contains("not a Hacksor session bundle"), "{err}");
    }

    #[test]
    fn an_unencrypted_bundle_needs_no_passphrase() {
        let sealed = seal(b"open", None).unwrap();
        assert_eq!(unseal(&sealed, None).unwrap(), b"open");
        // Passing one anyway is not an error: the file says how it was written.
        assert_eq!(unseal(&sealed, Some("ignored")).unwrap(), b"open");
    }

    #[test]
    fn reidentifying_replaces_every_mention_of_the_old_thread() {
        let rollout = "{\"id\":\"abc\"}\n{\"session_id\":\"abc\",\"x\":1}\n";
        let out = reidentify(rollout, "abc", "zzz");
        assert!(!out.contains("abc"), "{out}");
        assert_eq!(out.matches("zzz").count(), 2);
    }

    #[test]
    fn session_meta_and_model_are_read_from_the_rollout() {
        let rollout = concat!(
            r#"{"type":"session_meta","payload":{"id":"t1","cwd":"/work","model_provider":"opencodex","context_window":1000000}}"#,
            "\n",
            r#"{"type":"turn","payload":{"model":"anthropic/claude-opus-5"}}"#,
            "\n",
            r#"{"type":"turn","payload":{"model":"anthropic/claude-opus-5-fast"}}"#,
            "\n"
        );
        let meta = session_meta(rollout);
        assert_eq!(meta.cwd.as_deref(), Some("/work"));
        assert_eq!(meta.context_window, Some(1_000_000));
        // The last model wins: that is what the user was on when they exported.
        assert_eq!(model_from_rollout(rollout).as_deref(), Some("anthropic/claude-opus-5-fast"));
    }

    #[test]
    fn the_note_tells_the_model_what_moved_and_what_is_missing() {
        let note = mapping_note(
            &manifest(),
            "/home/b/work",
            &["target.exe".to_string()],
            &["notes.md".to_string()],
        );
        assert!(note.contains("/Users/a/work is /home/b/work"), "{note}");
        assert!(note.contains("target.exe"), "{note}");
        assert!(note.contains("notes.md"), "{note}");
        // It must not claim to be a summary: the history that follows is real.
        assert!(note.contains("not a summary"), "{note}");
    }

    #[test]
    fn an_unmoved_working_directory_produces_no_path_warning() {
        let note = mapping_note(&manifest(), "/Users/a/work", &[], &[]);
        assert!(!note.contains("Paths have moved"), "{note}");
    }

    #[test]
    fn the_note_record_parses_as_a_rollout_line() {
        let line = note_record("hello", "2026-11-26T00:00:00Z", 42);
        let v: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(v["payload"]["role"], "user");
        assert_eq!(v["payload"]["content"][0]["text"], "hello");
        assert!(!line.contains('\n'), "a rollout line must not contain a newline");
        // Without this codex refuses the whole thread: "final paginated rollout
        // record is missing an ordinal".
        assert_eq!(v["ordinal"], 42);
    }

    #[test]
    fn the_appended_note_continues_the_ordinal_sequence() {
        let rollout = concat!(
            r#"{"ordinal":7,"type":"event_msg","payload":{}}"#,
            "\n",
            r#"{"ordinal":8,"type":"event_msg","payload":{}}"#,
            "\n"
        );
        assert_eq!(last_ordinal(rollout), 8);
        // A rollout with no ordinals at all must still produce a usable one.
        assert_eq!(last_ordinal("{\"type\":\"x\"}\n"), 0);
    }

    #[test]
    fn a_newer_bundle_format_is_refused_with_advice() {
        let mut m = manifest();
        m.schema = SCHEMA + 1;
        let mut tar = tar::Builder::new(Vec::new());
        for (name, body) in [
            ("manifest.json", serde_json::to_vec(&m).unwrap()),
            ("thread.jsonl", b"{}\n".to_vec()),
        ] {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, name, body.as_slice()).unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &tar.into_inner().unwrap()).unwrap();
        let err = unpack(&gz.finish().unwrap()).unwrap_err().to_string();
        assert!(err.contains("newer Hacksor"), "{err}");
    }

    /// The whole chain against a REAL conversation from this machine, left on
    /// disk for a real codex to resume. Ignored by default: it needs the app's
    /// own data, so it proves the feature on a developer's machine rather than
    /// in CI.
    #[test]
    #[ignore]
    fn real_round_trip_for_manual_resume() {
        let home = dirs::home_dir().unwrap();
        let codex_home = home.join("Library/Application Support/dev.hacksor.app/codex-home");
        let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
        let mut stack = vec![codex_home.join("sessions")];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                    let t = e.metadata().and_then(|m| m.modified()).unwrap();
                    if newest.as_ref().map(|(bt, _)| t > *bt).unwrap_or(true) {
                        newest = Some((t, p));
                    }
                }
            }
        }
        let (_, rollout_path) = newest.expect("no real conversation found to export");
        let rollout = std::fs::read_to_string(&rollout_path).unwrap();
        let meta = session_meta(&rollout);
        let old_id = serde_json::from_str::<serde_json::Value>(rollout.lines().next().unwrap())
            .ok()
            .and_then(|v| {
                v.get("payload")
                    .unwrap_or(&v)
                    .get("id")
                    .and_then(|x| x.as_str())
                    .map(String::from)
            })
            .unwrap_or_default();

        let m = Manifest {
            schema: SCHEMA,
            app_version: "test".into(),
            exported_at: chrono::Utc::now().to_rfc3339(),
            source_os: "macos".into(),
            thread_id: old_id,
            title: "real".into(),
            model: model_from_rollout(&rollout),
            provider: meta.provider.clone(),
            context_window: meta.context_window,
            cwd: meta.cwd.clone(),
            engagement: None,
            counts: Counts::default(),
        };

        // Under the home, because the runtime container can only see that.
        let sandbox = home.join(".hacksor/portable-selftest");
        let _ = std::fs::remove_dir_all(&sandbox);
        let dest_home = sandbox.join("codex-home");
        let cwd = sandbox.join("work");
        std::fs::create_dir_all(&cwd).unwrap();

        let (archive, _) = build_bundle(&rollout_path, m, None, &[], None).unwrap();
        let sealed = seal(&archive, Some("selftest-passphrase")).unwrap();
        let bundle_path = sandbox.join("session.hacksor");
        std::fs::write(&bundle_path, &sealed).unwrap();

        let opened = unseal(&std::fs::read(&bundle_path).unwrap(), Some("selftest-passphrase")).unwrap();
        let unpacked = unpack(&opened).unwrap();
        let summary = install(&unpacked, &dest_home, &cwd, &sandbox.join(".hacksor")).unwrap();

        println!("SELFTEST_BUNDLE_BYTES {}", sealed.len());
        println!("SELFTEST_SOURCE {}", rollout_path.display());
        println!("SELFTEST_CODEX_HOME {}", dest_home.display());
        println!("SELFTEST_NEW_ID {}", summary.thread_id);
        println!("SELFTEST_MODEL {:?}", summary.model);
    }

    #[test]
    fn installing_leaves_a_resumable_thread_and_touches_nothing_else() {
        let dir = std::env::temp_dir().join(format!("hacksor-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let codex_home = dir.join("codex-home");
        let cwd = dir.join("work");
        let hacksor_home = dir.join(".hacksor");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&hacksor_home).unwrap();

        // Memory this machine already has: the import must add, not replace.
        std::fs::write(hacksor_home.join("lessons.jsonl"), "{\"local\":1}\n").unwrap();

        let rollout = concat!(
            r#"{"type":"session_meta","payload":{"id":"old-id","cwd":"/Users/a/work"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"find the bug"}]}}"#,
            "\n"
        );
        let bundle = Unpacked {
            manifest: manifest(),
            rollout: rollout.to_string(),
            workspace: vec![WorkspaceEntry {
                path: "target.bin".into(),
                size: 10,
                sha256: Some("deadbeef".into()),
            }],
            artifacts: vec![("decompiled/main.c".to_string(), b"int main(){}".to_vec())],
            engagement: vec![("ENGAGEMENT_NOTES.md".to_string(), b"# notes".to_vec())],
            memory: vec![("lessons.jsonl".to_string(), b"{\"from\":\"bundle\"}\n".to_vec())],
        };

        let s = install(&bundle, &codex_home, &cwd, &hacksor_home).unwrap();

        // A new identity, so importing cannot overwrite an existing chat.
        assert_ne!(s.thread_id, "old-id");
        assert_eq!(s.title, "Imported: A session");
        assert_eq!(s.model.as_deref(), Some("anthropic/claude-opus-5"));
        // The target file is absent here, and the summary says so rather than
        // letting the model assume the history still describes this machine.
        assert_eq!(s.missing_files, vec!["target.bin".to_string()]);

        // The conversation is where codex looks for threads.
        let mut found = None;
        let mut stack = vec![codex_home.join("sessions")];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() { stack.push(p); } else { found = Some(p); }
            }
        }
        let written = std::fs::read_to_string(found.expect("a rollout was written")).unwrap();
        assert!(written.contains(&s.thread_id), "the new id must be in the file");
        assert!(!written.contains("old-id"), "no trace of the old id may remain");
        assert!(written.contains("find the bug"), "the conversation must survive");
        // The last line is the note, so it is the freshest thing the model reads.
        let last = written.trim_end().lines().last().unwrap();
        assert!(last.contains("imported session"), "{last}");
        assert!(last.contains("target.bin"), "the note must name what is missing");
        serde_json::from_str::<serde_json::Value>(last).expect("the note is a valid record");

        // Artifacts restored, apart from the user's own files.
        assert_eq!(s.restored_artifacts, 1);
        assert_eq!(
            std::fs::read_to_string(cwd.join(".hacksor-imported/decompiled/main.c")).unwrap(),
            "int main(){}"
        );

        // Memory appended, with what was already here kept.
        let lessons = std::fs::read_to_string(hacksor_home.join("lessons.jsonl")).unwrap();
        assert!(lessons.contains("\"local\":1"), "existing memory was lost: {lessons}");
        assert!(lessons.contains("\"from\":\"bundle\""), "{lessons}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_imported_chat_is_named_so_the_user_can_see_where_it_came_from() {
        assert_eq!(imported_title("Recon on acme"), "Imported: Recon on acme");
        // An empty title still has to produce something selectable.
        assert_eq!(imported_title("   "), "Imported session");
        // Re-exporting an imported chat must not stack prefixes.
        assert_eq!(imported_title("Imported: Recon"), "Imported: Recon");
    }

    #[test]
    fn an_engagement_that_would_collide_is_renamed_not_merged() {
        let dir = std::env::temp_dir().join(format!("hacksor-collide-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hacksor_home = dir.join(".hacksor");
        let existing = hacksor_home.join("engagements").join("acme");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("ENGAGEMENT_NOTES.md"), "mine").unwrap();

        let mut m = manifest();
        m.engagement = Some("acme".into());
        let bundle = Unpacked {
            manifest: m,
            rollout: "{\"type\":\"session_meta\",\"payload\":{\"id\":\"old-id\"}}\n".to_string(),
            workspace: vec![],
            artifacts: vec![],
            engagement: vec![("ENGAGEMENT_NOTES.md".to_string(), b"theirs".to_vec())],
            memory: vec![],
        };
        let s = install(&bundle, &dir.join("codex-home"), &dir.join("work"), &hacksor_home).unwrap();

        assert_eq!(s.engagement.as_deref(), Some("acme-imported"));
        assert_eq!(std::fs::read_to_string(existing.join("ENGAGEMENT_NOTES.md")).unwrap(), "mine");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_and_unpack_preserve_the_conversation() {
        let dir = std::env::temp_dir().join(format!("hacksor-portable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rollout_path = dir.join("rollout.jsonl");
        let rollout = concat!(
            r#"{"type":"session_meta","payload":{"id":"t1","cwd":"/work"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"role":"user","content":[]}}"#,
            "\n"
        );
        std::fs::write(&rollout_path, rollout).unwrap();

        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("notes.md"), "# findings").unwrap();
        std::fs::write(work.join("target.bin"), vec![0u8; 4096]).unwrap();

        let (archive, m) =
            build_bundle(&rollout_path, manifest(), None, &[], Some(&work)).unwrap();
        let sealed = seal(&archive, Some("pw")).unwrap();
        let back = unpack(&unseal(&sealed, Some("pw")).unwrap()).unwrap();

        assert_eq!(back.rollout, rollout, "the conversation must survive exactly");
        assert_eq!(m.counts.rollout_lines, 2);
        // The text artifact rides along; the binary is described, not carried.
        assert!(back.artifacts.iter().any(|(n, _)| n == "notes.md"));
        assert!(!back.artifacts.iter().any(|(n, _)| n == "target.bin"));
        assert_eq!(back.workspace.len(), 2);
        let bin = back.workspace.iter().find(|e| e.path == "target.bin").unwrap();
        assert_eq!(bin.size, 4096);
        assert!(bin.sha256.is_some(), "a target file must be identifiable by hash");

        // And the hashes are what let the other machine tell same from different.
        let (missing, changed) = check_workspace(&back.workspace, &work);
        assert!(missing.is_empty() && changed.is_empty(), "{missing:?} {changed:?}");
        std::fs::write(work.join("target.bin"), vec![1u8; 4096]).unwrap();
        let (_, changed) = check_workspace(&back.workspace, &work);
        assert_eq!(changed, vec!["target.bin".to_string()]);
        std::fs::remove_file(work.join("target.bin")).unwrap();
        let (missing, _) = check_workspace(&back.workspace, &work);
        assert_eq!(missing, vec!["target.bin".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
