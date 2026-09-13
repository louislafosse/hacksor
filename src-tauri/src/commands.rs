//! Tauri command surface exposed to the Hacksor frontend.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::Mutex;

use crate::harness::{Harness, Permission, StartParams, TurnOverrides};
use crate::models::{self, ModelInfo, Provider, ProviderInfo};
use crate::settings::Settings;

/// App-wide state: the settings store and the lazily-spawned app-server client.
pub struct AppState {
    pub settings: Mutex<Settings>,
    pub harness: Mutex<Option<Arc<Harness>>>,
    pub codex_home: PathBuf,
    pub developer_prompt: String,
    /// Dynamic environment capabilities (skills path + detected local tools).
    pub env_note: String,
    /// Managed OpenCodex proxy process (translates Responses -> any provider).
    pub opencodex: Mutex<Option<tokio::process::Child>>,
}

use crate::platform;
use crate::runtime;

fn which_bin(tool: &str) -> Option<String> {
    platform::which(tool)
}

async fn opencodex_up() -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    else {
        return false;
    };
    match client.get(models::Provider::OpenCodex.models_url()).send().await {
        Ok(r) => {
            let c = r.status().as_u16();
            c == 200 || c == 401 // reachable (200) or auth-required (401) both mean "up"
        }
        Err(_) => false,
    }
}

/// True when the runtime is Docker (ocx runs inside the container) rather than
/// on the host.
async fn docker_runtime(state: &AppState) -> bool {
    state.settings.lock().await.runtime == "docker"
}

/// Push the stored OpenRouter/Vercel keys into OpenCodex as upstream providers
/// (both route through the proxy). Idempotent `provider add --force`; a removed
/// key removes the upstream. Runs against host ocx or the container's ocx per
/// mode. Returns true if anything was (re)configured.
async fn configure_ocx_upstreams(state: &AppState) -> bool {
    let docker = docker_runtime(state).await;
    let keys: Vec<(&'static str, Option<String>)> = {
        let s = state.settings.lock().await;
        crate::models::Provider::ALL
            .iter()
            .filter_map(|p| p.ocx_upstream().map(|u| (u, s.key_for(*p).map(String::from))))
            .collect()
    };
    let mut changed = false;
    for (upstream, key) in keys {
        match key {
            Some(k) if !k.is_empty() => {
                if run_ocx(&["provider", "add", upstream, "--api-key", &k, "--force"], None, docker).await.is_ok() {
                    changed = true;
                }
            }
            _ => {
                if run_ocx(&["provider", "remove", upstream], None, docker).await.is_ok() {
                    changed = true;
                }
            }
        }
    }
    changed
}

/// Restart the OpenCodex proxy so it re-reads its provider config (ocx only
/// loads providers at startup), then bring it back up.
async fn restart_opencodex(state: &AppState) {
    if docker_runtime(state).await {
        // In-container ocx: kill it; ensure_opencodex restarts it after config.
        if runtime_container_running().await {
            let _ = runtime::exec_output(RUNTIME_CONTAINER, vec!["pkill".into(), "-f".into(), "opencodex".into()], None, None).await;
        }
    } else {
        if let Some(mut child) = state.opencodex.lock().await.take() {
            let _ = child.start_kill();
        }
        if let Some(home) = dirs::home_dir() {
            if let Ok(pid) = std::fs::read_to_string(home.join(".opencodex/ocx.pid")) {
                if let Ok(pid) = pid.trim().parse::<i64>() {
                    platform::kill_pid(pid);
                }
            }
        }
    }
    for _ in 0..15 {
        if !opencodex_up().await {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let _ = ensure_opencodex(state).await;
}

/// Ensure the OpenCodex proxy is running and ready. In Docker mode it runs
/// INSIDE the bundled container (nothing to install on the host); in host mode
/// it's the host `ocx`. Upstreams are configured from the stored keys BEFORE the
/// server starts so it serves OpenRouter/Vercel/… without a further restart.
async fn ensure_opencodex(state: &AppState) -> Result<(), String> {
    if opencodex_up().await {
        return Ok(());
    }
    if docker_runtime(state).await {
        // The container hosts ocx (bundled). Make sure it's up, configure the
        // upstreams, then (re)start ocx in-container so it picks up the config.
        // Services pre-start is handled by ensure_runtime_ready; here we only need
        // the container up for ocx, so don't force the browser/proxy to start.
        start_runtime(&state.codex_home, false).await?;
        configure_ocx_upstreams(state).await;
        // Start ocx detached as the exec's own process (no shell wrapper — a
        // backgrounded `&` under a shell that exits would SIGHUP the proxy).
        let _ = runtime::exec_detached(
            RUNTIME_CONTAINER,
            vec!["ocx".into(), "start".into(), "--port".into(), models::OPENCODEX_PORT.to_string()],
            platform::docker_exec_user(),
        )
        .await;
        // Docker Desktop (macOS/Windows): ocx binds only to 127.0.0.1 inside the
        // container, so the published host port (which forwards to the container's
        // external interface) can't reach it and the proxy never looks "ready".
        // Bridge the container's own IP to ocx's loopback with socat so both the
        // published port and in-container clients reach it. On Linux the container
        // shares the host's 127.0.0.1 (host networking), so no bridge is needed.
        if !platform::IS_LINUX {
            let port = models::OPENCODEX_PORT;
            let _ = runtime::exec_detached(
                RUNTIME_CONTAINER,
                vec![
                    "sh".into(),
                    "-c".into(),
                    format!(
                        "socat TCP-LISTEN:{port},fork,reuseaddr,bind=$(hostname -i | awk '{{print $1}}') TCP:127.0.0.1:{port}"
                    ),
                ],
                platform::docker_exec_user(),
            )
            .await;
        }
        for _ in 0..24 {
            if opencodex_up().await {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        return Err("OpenCodex proxy in the runtime container didn't become ready.".into());
    }

    // Host mode: configure + spawn the host ocx.
    configure_ocx_upstreams(state).await;
    let bin = which_bin("ocx").ok_or_else(|| {
        "OpenCodex is not installed on the host. Switch Runtime to Docker (it's bundled there), or run `npm i -g @bitkyc08/opencodex`.".to_string()
    })?;
    let child = tokio::process::Command::new(bin)
        .args(["start", "--port", &models::OPENCODEX_PORT.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to start OpenCodex proxy: {e}"))?;
    *state.opencodex.lock().await = Some(child);
    for _ in 0..24 {
        if opencodex_up().await {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    Err("OpenCodex proxy did not become ready. Check `ocx` and your OpenCodex config.".into())
}

#[derive(Serialize)]
pub struct OpenCodexStatus {
    pub installed: bool,
    pub running: bool,
}

#[tauri::command]
pub async fn opencodex_status(state: State<'_, AppState>) -> Result<OpenCodexStatus, String> {
    // In Docker mode ocx is bundled in the runtime image; in host mode it must be
    // on the host PATH.
    let installed = if docker_runtime(&state).await {
        runtime_image_exists().await
    } else {
        which_bin("ocx").is_some()
    };
    Ok(OpenCodexStatus { installed, running: opencodex_up().await })
}

#[tauri::command]
pub async fn ensure_opencodex_cmd(state: State<'_, AppState>) -> Result<(), String> {
    ensure_opencodex(&state).await
}

/// Open OpenCodex's setup GUI (`ocx gui`) so the user can add providers/keys.
#[tauri::command]
pub fn open_opencodex_setup() -> Result<(), String> {
    let bin = which_bin("ocx")
        .ok_or_else(|| "OpenCodex is not installed. Run `npm i -g @bitkyc08/opencodex`.".to_string())?;
    // spawn_detached wraps Windows `.cmd`/`.bat` shims in `cmd /C` (CreateProcess
    // can't launch them directly), so the dashboard opens on every platform.
    platform::spawn_detached(&bin, &["gui"]).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn list_providers() -> Vec<ProviderInfo> {
    models::provider_list()
}

// ---- OpenCodex provider wrapper: set up any upstream provider from the UI ----

/// Run the `ocx` CLI with args, optionally piping `stdin_data` (for keys). In
/// Docker mode it runs the container's bundled ocx (`docker exec … ocx …`); in
/// host mode the host `ocx`.
async fn run_ocx(args: &[&str], stdin_data: Option<&str>, docker: bool) -> Result<String, String> {
    if docker {
        if !runtime_container_running().await {
            return Err("Runtime container is not running.".into());
        }
        let mut cmd: Vec<String> = vec!["ocx".into()];
        cmd.extend(args.iter().map(|s| s.to_string()));
        let (out, code) = runtime::exec_output(RUNTIME_CONTAINER, cmd, platform::docker_exec_user(), stdin_data).await?;
        if code == 0 {
            Ok(out)
        } else {
            Err(out.trim().to_string())
        }
    } else {
        let bin = which_bin("ocx").ok_or_else(|| {
            "OpenCodex (ocx) is not installed on the host. Use the Docker runtime (it's bundled there) or `npm i -g @bitkyc08/opencodex`.".to_string()
        })?;
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(if stdin_data.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() });
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        if let Some(data) = stdin_data {
            use tokio::io::AsyncWriteExt;
            if let Some(mut si) = child.stdin.take() {
                let _ = si.write_all(data.as_bytes()).await;
            }
        }
        let out = child.wait_with_output().await.map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).to_string())
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            let err = if err.trim().is_empty() { String::from_utf8_lossy(&out.stdout).to_string() } else { err.to_string() };
            Err(err.trim().to_string())
        }
    }
}

/// List providers already configured in OpenCodex (raw `ocx provider list`).
#[tauri::command]
pub async fn ocx_provider_list(state: State<'_, AppState>) -> Result<String, String> {
    let docker = docker_runtime(&state).await;
    run_ocx(&["provider", "list"], None, docker).await
}

/// Configure an upstream provider and store an API key for it. `provider` is a
/// registry name (openrouter, openai, anthropic, google, groq, deepseek, …).
#[tauri::command]
pub async fn ocx_add_provider(state: State<'_, AppState>, provider: String, key: String) -> Result<(), String> {
    let docker = docker_runtime(&state).await;
    let name = provider.trim().to_lowercase();
    let key = key.trim().to_string();
    // Registry providers configure in one step with an inline key.
    if key.is_empty() {
        run_ocx(&["provider", "add", &name], None, docker).await?;
    } else {
        run_ocx(&["provider", "add", &name, "--api-key", &key, "--force"], None, docker).await?;
    }
    // ocx only reads provider config at startup — restart so the new upstream serves.
    restart_opencodex(&state).await;
    Ok(())
}

/// Make a configured provider the default upstream for OpenCodex.
#[tauri::command]
pub async fn ocx_set_default(state: State<'_, AppState>, provider: String) -> Result<(), String> {
    let docker = docker_runtime(&state).await;
    let name = provider.trim().to_lowercase();
    run_ocx(&["provider", "set-default", &name], None, docker).await.map(|_| ())
}

#[derive(Deserialize)]
pub struct VerifyArgs {
    pub provider: String,
    pub model: String,
    pub task: String,
    pub answer: String,
}

#[derive(Serialize)]
pub struct Verdict {
    pub verdict: String, // "ok" | "partial" | "fail" | "unknown"
    pub reason: String,
}

/// Lightweight evaluator (AutoMix-style verifier gate) for the smart Auto loop.
/// Judges whether the agent's result accomplished the task. Fail-safe: any error
/// yields verdict "unknown" so the caller simply does not escalate.
#[tauri::command]
pub async fn verify_turn(state: State<'_, AppState>, args: VerifyArgs) -> Result<Verdict, String> {
    let p = Provider::parse(&args.provider);
    let key = {
        let s = state.settings.lock().await;
        s.key_for(p).map(|k| k.to_string())
    };
    let key = match key {
        Some(k) if !k.is_empty() => k,
        _ => return Ok(Verdict { verdict: "unknown".into(), reason: String::new() }),
    };

    let sys = "You are a strict evaluator for an autonomous security/pentest agent. Given the TASK and the agent's RESULT, decide whether the task was accomplished. Reply with ONLY compact JSON, no prose: {\"verdict\":\"ok\"|\"partial\"|\"fail\",\"reason\":\"<=140 chars\"}. ok = fully accomplished with concrete evidence. partial = real progress but incomplete or unverified. fail = did not accomplish, got stuck, refused, or errored out.";
    let user = format!(
        "TASK:\n{}\n\nRESULT:\n{}",
        truncate(&args.task, 4000),
        truncate(&args.answer, 6000)
    );
    let body = serde_json::json!({
        "model": args.model,
        "messages": [
            {"role": "system", "content": sys},
            {"role": "user", "content": user}
        ],
        "temperature": 0,
        "max_tokens": 200,
    });

    let client = match reqwest::Client::builder().user_agent("hacksor/0.1").build() {
        Ok(c) => c,
        Err(_) => return Ok(Verdict { verdict: "unknown".into(), reason: String::new() }),
    };
    let url = format!("{}/chat/completions", p.base_url());
    let resp = client.post(url).bearer_auth(key).json(&body).send().await;
    let text = match resp {
        Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
        _ => return Ok(Verdict { verdict: "unknown".into(), reason: String::new() }),
    };
    let value: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    let content = value
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    Ok(parse_verdict(content))
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[derive(Deserialize)]
pub struct ReviewArgs {
    pub provider: String,
    pub model: String,
    pub command: String,
    #[serde(default)]
    pub context: Option<String>,
}

#[derive(Serialize)]
pub struct ReviewDecision {
    pub decision: String, // "approve" | "ask" | "deny"
    pub risk: String,
    pub reason: String,
}

/// Rule-based catastrophic-command detector for the auto-review fast path.
fn hard_deny(cmd: &str) -> Option<&'static str> {
    let c = cmd.to_lowercase();
    let danger = [
        ("rm -rf /", "recursive delete of the filesystem root"),
        ("rm -rf /*", "recursive delete of the filesystem root"),
        ("rm -rf ~", "recursive delete of the home directory"),
        ("rm -rf $home", "recursive delete of the home directory"),
        ("mkfs", "formatting a filesystem"),
        ("dd if=/dev/zero of=/dev/", "overwriting a block device"),
        ("dd of=/dev/sd", "overwriting a disk device"),
        (":(){ :|:& };:", "fork bomb"),
        ("> /dev/sda", "overwriting a disk device"),
        ("chmod -r 000 /", "removing all permissions from root"),
        ("mv /home", "moving the home directory"),
        ("shutdown", "shutting the machine down"),
        ("reboot", "rebooting the machine"),
    ];
    danger.iter().find(|(pat, _)| c.contains(pat)).map(|(_, why)| *why)
}

/// Auto-review reviewer: decide whether a pending approval-gated command should
/// be approved, denied, or escalated to the user. A rule-based fast path blocks
/// catastrophic commands; otherwise a reviewer model judges. Fail-safe: any
/// error yields "ask" so the user is prompted rather than auto-approving.
#[tauri::command]
pub async fn review_action(state: State<'_, AppState>, args: ReviewArgs) -> Result<ReviewDecision, String> {
    if let Some(why) = hard_deny(&args.command) {
        return Ok(ReviewDecision { decision: "deny".into(), risk: "destructive".into(), reason: format!("Blocked: {why}.") });
    }
    let p = Provider::parse(&args.provider);
    let key = {
        let s = state.settings.lock().await;
        s.key_for(p).map(|k| k.to_string())
    };
    let key = match key {
        Some(k) if !k.is_empty() => k,
        // No key (e.g. OpenCodex) → fall back to asking the user.
        _ => return Ok(ReviewDecision { decision: "ask".into(), risk: String::new(), reason: String::new() }),
    };
    let sys = "You are a strict security reviewer approving actions for an AUTHORIZED penetration-testing agent operating on the user's own machine. Offensive security actions (recon, scanning, exploitation, cred testing, payload generation) are IN SCOPE and should be APPROVED. Your job is to catch actions that are catastrophic or clearly outside a pentest: destroying the user's own system/data, exfiltrating the user's private data to third parties, disabling the user's own security, or irreversible unrelated damage. Reply with ONLY compact JSON: {\"decision\":\"approve\"|\"ask\"|\"deny\",\"risk\":\"destructive|credential_access|data_egress|security_weakening|scope_expansion|none\",\"reason\":\"<=120 chars\"}. approve = safe/in-scope. ask = genuinely ambiguous, defer to human. deny = catastrophic or clearly out of scope.";
    let user = format!("COMMAND:\n{}\n\nCONTEXT:\n{}", truncate(&args.command, 3000), truncate(args.context.as_deref().unwrap_or(""), 2000));
    let body = serde_json::json!({
        "model": args.model,
        "messages": [{"role":"system","content":sys},{"role":"user","content":user}],
        "temperature": 0, "max_tokens": 160,
    });
    let client = match reqwest::Client::builder().user_agent("hacksor/0.1").build() {
        Ok(c) => c,
        Err(_) => return Ok(ReviewDecision { decision: "ask".into(), risk: String::new(), reason: String::new() }),
    };
    let url = format!("{}/chat/completions", p.base_url());
    let resp = client.post(url).bearer_auth(key).json(&body).send().await;
    let text = match resp {
        Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
        _ => return Ok(ReviewDecision { decision: "ask".into(), risk: String::new(), reason: String::new() }),
    };
    let value: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    let content = value.get("choices").and_then(|c| c.get(0)).and_then(|c| c.get("message")).and_then(|m| m.get("content")).and_then(|c| c.as_str()).unwrap_or("");
    Ok(parse_review(content))
}

fn parse_review(content: &str) -> ReviewDecision {
    if let (Some(a), Some(b)) = (content.find('{'), content.rfind('}')) {
        if b > a {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content[a..=b]) {
                let d = v.get("decision").and_then(|x| x.as_str()).unwrap_or("ask");
                let decision = match d { "approve" | "deny" | "ask" => d, _ => "ask" };
                return ReviewDecision {
                    decision: decision.to_string(),
                    risk: v.get("risk").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    reason: v.get("reason").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                };
            }
        }
    }
    ReviewDecision { decision: "ask".into(), risk: String::new(), reason: String::new() }
}

fn parse_verdict(content: &str) -> Verdict {
    // Extract the first {...} JSON object from the reply.
    if let (Some(a), Some(b)) = (content.find('{'), content.rfind('}')) {
        if b > a {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content[a..=b]) {
                let verdict = v.get("verdict").and_then(|x| x.as_str()).unwrap_or("unknown");
                let verdict = match verdict {
                    "ok" | "partial" | "fail" => verdict,
                    _ => "unknown",
                };
                let reason = v.get("reason").and_then(|x| x.as_str()).unwrap_or("").to_string();
                return Verdict { verdict: verdict.to_string(), reason };
            }
        }
    }
    Verdict { verdict: "unknown".into(), reason: String::new() }
}

/// Fetch the live model catalog for a provider from its `/models` endpoint.
#[tauri::command]
pub async fn list_models(
    state: State<'_, AppState>,
    provider: String,
) -> Result<Vec<ModelInfo>, String> {
    let p = Provider::parse(&provider);
    let key = {
        let s = state.settings.lock().await;
        s.key_for(p).map(|k| k.to_string())
    };
    if p.models_need_key() && key.as_deref().map(|k| k.is_empty()).unwrap_or(true) {
        return Err(format!(
            "{} needs an API key before its models can be listed. Add it in Settings.",
            p.display()
        ));
    }
    if p.is_local_proxy() {
        ensure_opencodex(&state).await?;
    }
    models::fetch_models(p, key.as_deref())
        .await
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
pub struct SettingsView {
    pub provider: String,
    pub has_openrouter_key: bool,
    pub has_vercel_key: bool,
    pub personality: Option<String>,
    pub custom_instructions: Option<String>,
    pub working_dir: String,
    pub kali_mode: bool,
    pub runtime: String,
    pub services_mode: String,
    pub has_cloudflare: bool,
    pub cloudflare_email: Option<String>,
    pub cloudflare_account_id: Option<String>,
}

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> Result<SettingsView, String> {
    let s = state.settings.lock().await;
    Ok(SettingsView {
        provider: s.provider.clone(),
        has_openrouter_key: s.key_for(Provider::OpenRouter).is_some(),
        has_vercel_key: s.key_for(Provider::Vercel).is_some(),
        personality: s.personality.clone(),
        custom_instructions: s.custom_instructions.clone(),
        working_dir: s.working_dir.clone(),
        kali_mode: s.kali_mode,
        runtime: s.runtime.clone(),
        services_mode: s.services_mode.clone(),
        has_cloudflare: s.cloudflare_api_key.as_deref().map(|k| !k.is_empty()).unwrap_or(false),
        cloudflare_email: s.cloudflare_email.clone(),
        cloudflare_account_id: s.cloudflare_account_id.clone(),
    })
}

#[derive(Deserialize)]
pub struct SaveSettingsArgs {
    pub provider: Option<String>,
    pub openrouter_api_key: Option<String>,
    pub vercel_api_key: Option<String>,
    pub personality: Option<String>,
    pub custom_instructions: Option<String>,
    pub working_dir: Option<String>,
    pub kali_mode: Option<bool>,
    pub runtime: Option<String>,
    pub services_mode: Option<String>,
    pub cloudflare_api_key: Option<String>,
    pub cloudflare_email: Option<String>,
    pub cloudflare_account_id: Option<String>,
}

#[tauri::command]
pub async fn save_settings(
    state: State<'_, AppState>,
    args: SaveSettingsArgs,
) -> Result<(), String> {
    let mut s = state.settings.lock().await;
    // Only an API-key or runtime change requires respawning the app-server (env
    // keys are captured at spawn; runtime changes the launch mode). A provider,
    // personality, working-dir or kali-mode change does NOT — restarting on those
    // needlessly drops the live harness and surfaced a scary "backend stopped"
    // notice when merely selecting a provider like OpenCodex.
    let mut needs_restart = false;
    let mut keys_changed = false;
    if let Some(p) = args.provider {
        s.provider = p;
    }
    if let Some(k) = args.openrouter_api_key {
        let t = k.trim().to_string();
        let v = if t.is_empty() { None } else { Some(t) };
        let changed = v != s.openrouter_api_key;
        needs_restart |= changed;
        keys_changed |= changed;
        s.openrouter_api_key = v;
    }
    if let Some(k) = args.vercel_api_key {
        let t = k.trim().to_string();
        let v = if t.is_empty() { None } else { Some(t) };
        let changed = v != s.vercel_api_key;
        needs_restart |= changed;
        keys_changed |= changed;
        s.vercel_api_key = v;
    }
    if let Some(p) = args.personality {
        s.personality = if p.trim().is_empty() { None } else { Some(p) };
    }
    if let Some(c) = args.custom_instructions {
        s.custom_instructions = if c.trim().is_empty() { None } else { Some(c) };
    }
    if let Some(dir) = args.working_dir {
        if !dir.trim().is_empty() {
            s.working_dir = dir.trim().to_string();
        }
    }
    if let Some(k) = args.kali_mode {
        s.kali_mode = k;
    }
    if let Some(r) = args.runtime {
        needs_restart |= r != s.runtime;
        s.runtime = r;
    }
    if let Some(m) = args.services_mode {
        // No harness restart: this only changes what the NEXT runtime prep starts.
        s.services_mode = m;
    }
    // Cloudflare creds for the `cloudfish` recon tool. Empty string clears. No
    // harness restart — they're just mirrored to ~/.cloudflare on save() below.
    let cf_set = |cur: &mut Option<String>, v: Option<String>| {
        if let Some(v) = v { *cur = if v.trim().is_empty() { None } else { Some(v.trim().to_string()) }; }
    };
    cf_set(&mut s.cloudflare_api_key, args.cloudflare_api_key);
    cf_set(&mut s.cloudflare_email, args.cloudflare_email);
    cf_set(&mut s.cloudflare_account_id, args.cloudflare_account_id);
    s.save(&state.codex_home).map_err(|e| e.to_string())?;
    s.export_env();
    drop(s);

    // A changed OpenRouter/Vercel key now configures the OpenCodex upstream those
    // providers route through, then restarts ocx so it picks up the change
    // (ocx only reads provider config at startup).
    if keys_changed {
        configure_ocx_upstreams(&state).await;
        restart_opencodex(&state).await;
    }

    // Restart the app-server only when a key or the runtime changed (env keys are
    // captured at spawn; runtime changes the launch mode). ensure_harness spawns a
    // fresh one on the next command. A provider/persona/dir change keeps the live
    // harness (the provider is passed per-thread at thread/start).
    if needs_restart {
        let existing = state.harness.lock().await.take();
        if let Some(h) = existing {
            h.shutdown().await;
        }
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct StartChatArgs {
    pub provider: String,
    pub model: String, // provider slug
    pub mode: String,   // "agent" | "ask"
    pub permission: String,
    pub working_dir: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
}

#[tauri::command]
pub async fn start_chat(
    app: AppHandle,
    state: State<'_, AppState>,
    args: StartChatArgs,
) -> Result<String, String> {
    let provider = Provider::parse(&args.provider);

    let (default_working_dir, personality, kali_mode, custom) = {
        let s = state.settings.lock().await;
        s.export_env();
        // OpenRouter/Vercel now route through the OpenCodex proxy, but they still
        // need their own key (Hacksor pushes it into ocx as that upstream). The
        // OpenCodex selection itself has no Hacksor-side key (its upstreams are
        // configured in ocx directly).
        if provider.ocx_upstream().is_some() && s.key_for(provider).is_none() {
            return Err(format!(
                "No {} API key set. Open Settings and add your key.",
                provider.display()
            ));
        }
        (s.working_dir.clone(), s.personality.clone(), s.kali_mode, s.custom_instructions.clone())
    };

    // Every provider is served by the local OpenCodex proxy now.
    ensure_opencodex(&state).await?;

    let harness = ensure_harness(&state, &app).await?;

    let working_dir = args
        .working_dir
        .filter(|d| !d.trim().is_empty())
        .unwrap_or(default_working_dir);
    let host_cwd = PathBuf::from(&working_dir);
    if !host_cwd.is_dir() {
        return Err(format!("Working directory does not exist: {working_dir}"));
    }
    // In Docker mode the codex thread's cwd is the CONTAINER path (identity on
    // Linux/macOS; remapped under the Windows home mount). In host mode it's the
    // host path unchanged.
    let docker_mode = { state.settings.lock().await.runtime == "docker" };
    let cwd = if docker_mode {
        PathBuf::from(platform::container_working_dir(&host_cwd))
    } else {
        host_cwd.clone()
    };

    // Kali mode auto-starts its container (idempotent) so the user never has to
    // start it by hand — the persona then runs tools via `docker exec hacksor-kali`.
    if kali_mode {
        let _ = start_kali(working_dir.clone()).await;
    }

    let permission = if args.mode == "ask" {
        Permission::ReadOnly
    } else {
        Permission::parse(&args.permission)
    };

    let developer_instructions = build_developer_prompt(
        &effective_persona(&state),
        &state.env_note,
        &args.mode,
        &args.model,
        personality.as_deref(),
        args.role.as_deref().unwrap_or("default"),
        if kali_mode { Some(working_dir.as_str()) } else { None },
        custom.as_deref(),
    );

    harness
        .start_thread(StartParams {
            provider,
            model_slug: args.model.clone(),
            cwd,
            permission,
            developer_instructions,
        })
        .await
        .map_err(|e| e.to_string())
}

#[derive(Deserialize)]
pub struct SendArgs {
    pub thread_id: String,
    pub text: String,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub images: Vec<String>,
    /// For edit-and-resend: how many trailing turns to roll back before sending.
    #[serde(default)]
    pub turns: Option<i64>,
}

/// Build the per-turn overrides, always including the HackerAI persona so it is
/// re-asserted on every turn (across model/provider switches and resumes).
fn turn_overrides_from(state: &AppState, args: &SendArgs, personality: Option<&str>, kali_mode: bool, custom: Option<&str>, docker_mode: bool) -> TurnOverrides {
    let dev = args.model.as_deref().map(|model| {
        build_developer_prompt(
            &effective_persona(state),
            &state.env_note,
            args.mode.as_deref().unwrap_or("agent"),
            model,
            personality,
            args.role.as_deref().unwrap_or("default"),
            if kali_mode { args.working_dir.as_deref() } else { None },
            custom,
        )
    });
    // In Docker mode the per-turn cwd is the container path (identity off-Windows).
    let cwd = args.working_dir.as_ref().map(|w| {
        if docker_mode {
            platform::container_working_dir(std::path::Path::new(w))
        } else {
            w.clone()
        }
    });
    TurnOverrides {
        cwd,
        model: args.model.clone(),
        provider: args.provider.as_deref().map(|p| Provider::parse(p).id().to_string()),
        effort: args.effort.clone(),
        developer_instructions: dev,
    }
}

#[tauri::command]
pub async fn send_message(app: AppHandle, state: State<'_, AppState>, args: SendArgs) -> Result<(), String> {
    let (personality, kali_mode, custom, docker_mode) = {
        let s = state.settings.lock().await;
        s.export_env();
        (s.personality.clone(), s.kali_mode, s.custom_instructions.clone(), s.runtime == "docker")
    };
    let overrides = turn_overrides_from(&state, &args, personality.as_deref(), kali_mode, custom.as_deref(), docker_mode);
    let harness = ensure_harness(&state, &app).await?;
    harness
        .send_message(&args.thread_id, args.text, overrides, args.images)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn interrupt(app: AppHandle, state: State<'_, AppState>, thread_id: String) -> Result<(), String> {
    let harness = ensure_harness(&state, &app).await?;
    harness.interrupt(&thread_id).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn fork_thread(app: AppHandle, state: State<'_, AppState>, thread_id: String) -> Result<String, String> {
    let harness = ensure_harness(&state, &app).await?;
    harness.fork_thread(&thread_id).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn list_threads(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    // Do NOT spawn the app-server until at least one provider key exists: a child
    // process captures its environment at spawn time, so spawning key-less here
    // would leave the server unable to authenticate even after a key is saved.
    {
        let s = state.settings.lock().await;
        s.export_env();
        let has_key =
            s.key_for(Provider::OpenRouter).is_some() || s.key_for(Provider::Vercel).is_some();
        if !has_key {
            return Ok(serde_json::json!([]));
        }
    }
    let harness = ensure_harness(&state, &app).await?;
    harness.list_threads().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn resume_thread(
    app: AppHandle,
    state: State<'_, AppState>,
    thread_id: String,
) -> Result<serde_json::Value, String> {
    let harness = ensure_harness(&state, &app).await?;
    harness.resume_thread(&thread_id).await.map_err(|e| e.to_string())
}

/// Find the codex rollout `.jsonl` for a thread (its id is in the file name).
fn find_rollout(codex_home: &std::path::Path, thread_id: &str) -> Option<std::path::PathBuf> {
    fn walk(dir: &std::path::Path, needle: &str, out: &mut Option<std::path::PathBuf>) {
        if out.is_some() {
            return;
        }
        if let Ok(rd) = std::fs::read_dir(dir) {
            let mut entries: Vec<_> = rd.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, needle, out);
                } else if let Some(n) = p.file_name().and_then(|s| s.to_str()) {
                    if n.ends_with(".jsonl") && n.contains(needle) {
                        *out = Some(p.clone());
                        return;
                    }
                }
            }
        }
    }
    let mut out = None;
    walk(&codex_home.join("sessions"), thread_id, &mut out);
    out
}

fn collect_text(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        Some(serde_json::Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// codex wraps exec output as "Chunk ID: …\n…\nOutput:\n<stdout>". Return the
/// stdout and the exit code.
fn parse_exec_output(raw: &str) -> (String, Option<i64>) {
    let code = raw.lines().find_map(|l| {
        l.strip_prefix("Process exited with code ")
            .and_then(|s| s.trim().parse::<i64>().ok())
    });
    let out = match raw.find("Output:\n") {
        Some(pos) => raw[pos + "Output:\n".len()..].to_string(),
        None => raw.to_string(),
    };
    (out, code)
}

/// Reconstruct the full transcript (messages + reasoning + commands with output)
/// from codex's rollout file. `thread/resume` drops reasoning and some commands,
/// so we parse the on-disk rollout — the ground truth — instead, letting the
/// thinking and command-execution blocks survive an app restart.
#[tauri::command]
pub fn read_transcript(
    state: State<'_, AppState>,
    thread_id: String,
) -> Result<Vec<serde_json::Value>, String> {
    let path = match find_rollout(&state.codex_home, &thread_id) {
        Some(p) => p,
        None => return Ok(vec![]),
    };
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    Ok(parse_rollout(&text))
}

/// The provider a thread was CREATED under, read from the rollout's
/// `session_meta.model_provider`. codex binds a thread to its creation provider
/// (turn/start & thread/resume ignore `modelProvider`), so the frontend uses
/// this to know when a provider switch must start a fresh thread. Returns the
/// codex provider id (matches Hacksor's ids: openrouter/vercel/opencodex), or
/// an empty string when unknown.
#[tauri::command]
pub fn thread_provider(state: State<'_, AppState>, thread_id: String) -> Result<String, String> {
    let path = match find_rollout(&state.codex_home, &thread_id) {
        Some(p) => p,
        None => return Ok(String::new()),
    };
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    Ok(provider_from_rollout(&text).unwrap_or_default())
}

/// Build a compact, bounded "conversation so far" block from a thread's rollout
/// so it can be replayed as context into a NEW thread when the provider changes
/// (codex binds a thread to its creation provider, so a provider switch starts a
/// fresh thread — this carries the history across it). Keeps user/assistant
/// messages and executed commands with truncated output (discovered ports,
/// endpoints, creds etc. are the context that matters for a pentest); drops
/// reasoning (verbose and provider-specific). Keeps the MOST RECENT content
/// within `max_chars`.
#[tauri::command]
pub fn build_carryover(
    state: State<'_, AppState>,
    thread_id: String,
    max_chars: Option<usize>,
) -> Result<String, String> {
    let cap = max_chars.unwrap_or(12_000).clamp(1_000, 60_000);
    let path = match find_rollout(&state.codex_home, &thread_id) {
        Some(p) => p,
        None => return Ok(String::new()),
    };
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    Ok(carryover_from_items(&parse_rollout(&text), cap))
}

/// Pure formatter for `build_carryover` (unit-tested).
fn carryover_from_items(items: &[serde_json::Value], cap: usize) -> String {
    // Render each turn to a line, then keep the most recent lines within cap.
    let mut lines: Vec<String> = Vec::new();
    for it in items {
        match it["type"].as_str() {
            Some("userMessage") => {
                let t = it["content"][0]["text"].as_str().unwrap_or("").trim();
                if !t.is_empty() {
                    lines.push(format!("User: {t}"));
                }
            }
            Some("agentMessage") => {
                let t = it["text"].as_str().unwrap_or("").trim();
                if !t.is_empty() {
                    lines.push(format!("Assistant: {t}"));
                }
            }
            Some("commandExecution") => {
                let cmd = it["command"].as_str().unwrap_or("").trim();
                if cmd.is_empty() {
                    continue;
                }
                let code = it["exitCode"].as_i64();
                let out = it["aggregatedOutput"].as_str().unwrap_or("");
                let out_short = truncate(out.trim(), 800);
                let mut block = format!("$ {cmd}");
                if let Some(c) = code {
                    block.push_str(&format!("  (exit {c})"));
                }
                if !out_short.is_empty() {
                    block.push_str(&format!("\n{out_short}"));
                }
                lines.push(block);
            }
            _ => {}
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    // Keep the most recent lines that fit within cap (measured on the joined body).
    let mut kept: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut total = 0usize;
    let mut truncated = false;
    for line in lines.into_iter().rev() {
        let add = line.chars().count() + 2;
        if total + add > cap && !kept.is_empty() {
            truncated = true;
            break;
        }
        total += add;
        kept.push_front(line);
    }
    let body: Vec<String> = kept.into_iter().collect();
    let head = if truncated {
        "This is the RECENT part of an ongoing conversation (earlier turns omitted), migrated because the provider/model changed. Continue it seamlessly using this context — do not re-introduce yourself, re-run completed steps, or repeat findings already established below."
    } else {
        "This is the conversation so far, migrated because the provider/model changed. Continue it seamlessly using this context — do not re-introduce yourself, re-run completed steps, or repeat findings already established below."
    };
    format!("<conversation_carryover>\n{head}\n\n{}\n</conversation_carryover>", body.join("\n"))
}

/// Recent OpenCodex request records (from `~/.opencodex/usage.jsonl`), newest
/// last. Each record has the authoritative served model/provider, the requested
/// model, HTTP status, duration and token usage — used by the per-answer Info
/// popover to show exactly which model answered and what it consumed.
#[tauri::command]
pub fn usage_recent(limit: Option<usize>) -> Result<Vec<serde_json::Value>, String> {
    let n = limit.unwrap_or(40).clamp(1, 500);
    let Some(home) = dirs::home_dir() else { return Ok(vec![]) };
    let path = home.join(".opencodex/usage.jsonl");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Ok(vec![]),
    };
    let mut recs: Vec<serde_json::Value> = text
        .lines()
        .rev()
        .take(n)
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect();
    recs.reverse();
    Ok(recs)
}

/// Extract `session_meta.model_provider` from a rollout's opening lines.
fn provider_from_rollout(text: &str) -> Option<String> {
    for line in text.lines().take(5) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v.get("type").and_then(|t| t.as_str()) == Some("session_meta") {
                if let Some(p) = v["payload"]["model_provider"].as_str() {
                    return Some(p.to_string());
                }
            }
        }
    }
    None
}

#[derive(Serialize)]
pub struct ChatSearchHit {
    pub thread_id: String,
    pub title: String,
    pub snippet: String,
}

/// Full-text search across ALL on-disk chats by grepping the rollout files.
/// Returns thread id, a title (first real user message) and a match snippet.
/// Async + `spawn_blocking`: the disk scan runs off the main thread so typing
/// in the search box never blocks the UI.
#[tauri::command]
pub async fn search_chats(state: State<'_, AppState>, query: String) -> Result<Vec<ChatSearchHit>, String> {
    let root = state.codex_home.join("sessions");
    tokio::task::spawn_blocking(move || search_chats_blocking(root, query))
        .await
        .map_err(|e| e.to_string())?
}

fn search_chats_blocking(root: std::path::PathBuf, query: String) -> Result<Vec<ChatSearchHit>, String> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Ok(vec![]);
    }
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    out.push(p);
                }
            }
        }
    }
    walk(&root, &mut files);
    // Newest first by mtime.
    files.sort_by_key(|p| std::cmp::Reverse(
        std::fs::metadata(p).and_then(|m| m.modified()).ok(),
    ));

    let mut hits = Vec::new();
    for path in files {
        if hits.len() >= 50 {
            break;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        if !text.to_lowercase().contains(&q) {
            continue;
        }
        // Reconstruct just the user/assistant text to build a title + snippet.
        let items = parse_rollout(&text);
        let mut title = String::new();
        let mut snippet = String::new();
        for it in &items {
            let body = match it["type"].as_str() {
                Some("userMessage") => it["content"][0]["text"].as_str().unwrap_or(""),
                Some("agentMessage") => it["text"].as_str().unwrap_or(""),
                _ => "",
            };
            if body.is_empty() {
                continue;
            }
            if title.is_empty() && it["type"] == "userMessage" {
                title = body.chars().take(60).collect();
            }
            if snippet.is_empty() {
                if let Some(pos) = body.to_lowercase().find(&q) {
                    let start = pos.saturating_sub(30);
                    snippet = body.chars().skip(start).take(110).collect();
                }
            }
        }
        // Filename is rollout-<ts…>-<uuid>.jsonl; the thread id is the trailing
        // UUID = the last five dash-separated segments (8-4-4-4-12).
        let thread_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|stem| {
                let segs: Vec<&str> = stem.split('-').collect();
                if segs.len() >= 5 {
                    segs[segs.len() - 5..].join("-")
                } else {
                    stem.to_string()
                }
            })
            .unwrap_or_default();
        if title.is_empty() {
            title = "Untitled chat".to_string();
        }
        hits.push(ChatSearchHit { thread_id, title, snippet });
    }
    Ok(hits)
}

#[derive(Serialize)]
pub struct RecentChat {
    pub id: String,
    pub preview: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: u64,
}

/// Read the first real user message from a rollout (the sidebar preview/title)
/// by streaming lines and stopping at the first hit — WITHOUT loading or parsing
/// the whole file. Rollouts can be many MB; this reads only the opening lines.
fn preview_from_rollout(path: &std::path::Path) -> String {
    use std::io::BufRead;
    let Ok(f) = std::fs::File::open(path) else { return String::new() };
    let reader = std::io::BufReader::new(f);
    for line in reader.lines().take(400).flatten() {
        let l = line.trim();
        if l.is_empty() || !l.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(l) else { continue };
        // codex stores messages under `payload` (response_item) — match a user
        // message whose text isn't one of the injected <…> wrapper blocks.
        let p = if v.get("payload").is_some() { &v["payload"] } else { &v };
        if p.get("type").and_then(|t| t.as_str()) == Some("message")
            && p.get("role").and_then(|r| r.as_str()) == Some("user")
        {
            let text = p["content"]
                .as_array()
                .and_then(|arr| arr.iter().find_map(|c| c.get("text").and_then(|t| t.as_str())))
                .unwrap_or("")
                .trim();
            if !text.is_empty() && !text.starts_with('<') {
                return text.chars().take(80).collect();
            }
        }
    }
    String::new()
}

/// List ALL on-disk chats for the Recents sidebar, read straight from the
/// rollout files (no harness / Docker needed — so recents always load, fast,
/// independent of the runtime). Newest first. This replaces routing Recents
/// through the app-server's `thread/list`, which was gated on the runtime being
/// up and could return a subset or nothing while Docker provisioned.
#[tauri::command]
pub fn list_recents(state: State<'_, AppState>) -> Result<Vec<RecentChat>, String> {
    let root = state.codex_home.join("sessions");
    let mut files: Vec<(std::path::PathBuf, u64)> = Vec::new();
    fn walk(dir: &std::path::Path, out: &mut Vec<(std::path::PathBuf, u64)>) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    let mtime = std::fs::metadata(&p)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    out.push((p, mtime));
                }
            }
        }
    }
    walk(&root, &mut files);
    files.sort_by_key(|(_, m)| std::cmp::Reverse(*m));

    let mut out = Vec::new();
    for (path, mtime) in files.into_iter().take(1000) {
        // Extract the preview by scanning ONLY until the first real user message
        // — never read/parse the whole file (rollouts can be many MB), so the
        // sidebar stays fast even with big conversations.
        let preview = preview_from_rollout(&path);
        let thread_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|stem| {
                let segs: Vec<&str> = stem.split('-').collect();
                if segs.len() >= 5 { segs[segs.len() - 5..].join("-") } else { stem.to_string() }
            })
            .unwrap_or_default();
        if thread_id.is_empty() {
            continue;
        }
        out.push(RecentChat { id: thread_id, preview, updated_at: mtime });
    }
    Ok(out)
}

fn parse_rollout(text: &str) -> Vec<serde_json::Value> {
    use serde_json::{json, Value};
    use std::collections::{HashMap, VecDeque};

    const SKIP_USER_PREFIXES: [&str; 7] = [
        "<environment_context",
        "<model_switch",
        "<permissions",
        "<skills_instructions",
        "<user_instructions",
        "<world_state",
        "<worldstate",
    ];

    let mut items: Vec<Value> = Vec::new();
    let mut pending: HashMap<String, VecDeque<usize>> = HashMap::new();
    let mut idx = 0usize;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let o: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let pay = o.get("payload").filter(|p| p.is_object()).unwrap_or(&o);
        let pt = pay.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match pt {
            "message" => {
                let role = pay.get("role").and_then(|v| v.as_str()).unwrap_or("");
                let content = collect_text(pay.get("content"));
                if content.trim().is_empty() || role == "developer" {
                    continue;
                }
                if role == "user" {
                    let t = content.trim_start();
                    if SKIP_USER_PREFIXES.iter().any(|p| t.starts_with(p)) {
                        continue;
                    }
                    items.push(json!({"type":"userMessage","id":format!("t{idx}"),"content":[{"type":"text","text":content}]}));
                    idx += 1;
                } else if role == "assistant" {
                    items.push(json!({"type":"agentMessage","id":format!("t{idx}"),"text":content}));
                    idx += 1;
                }
            }
            "reasoning" => {
                let mut parts: Vec<String> = Vec::new();
                for key in ["content", "summary"] {
                    if let Some(arr) = pay.get(key).and_then(|v| v.as_array()) {
                        for c in arr {
                            let t = c
                                .as_str()
                                .map(|s| s.to_string())
                                .or_else(|| c.get("text").and_then(|v| v.as_str()).map(|s| s.to_string()));
                            if let Some(t) = t {
                                if !t.trim().is_empty() {
                                    parts.push(t);
                                }
                            }
                        }
                    }
                }
                if parts.is_empty() {
                    continue;
                }
                items.push(json!({"type":"reasoning","id":format!("t{idx}"),"content":parts}));
                idx += 1;
            }
            "function_call" | "local_shell_call" | "custom_tool_call" => {
                let name = pay.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let call_id = pay
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let cmd = match pay.get("arguments") {
                    Some(Value::String(s)) => serde_json::from_str::<Value>(s)
                        .ok()
                        .and_then(|a| a.get("cmd").and_then(|c| c.as_str()).map(String::from))
                        .unwrap_or_else(|| s.clone()),
                    Some(Value::Object(_)) => pay
                        .get("arguments")
                        .and_then(|a| a.get("cmd"))
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string(),
                    _ => String::new(),
                };
                let display = if name.is_empty() || name == "exec_command" {
                    cmd
                } else {
                    format!("{name} {cmd}")
                };
                let pos = items.len();
                items.push(json!({"type":"commandExecution","id":format!("t{idx}"),"command":display,"aggregatedOutput":"","status":"completed"}));
                idx += 1;
                pending.entry(call_id).or_default().push_back(pos);
            }
            "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
                let call_id = pay
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let raw = match pay.get("output") {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => String::new(),
                };
                let (out, code) = parse_exec_output(&raw);
                if let Some(pos) = pending.get_mut(&call_id).and_then(|q| q.pop_front()) {
                    if let Some(obj) = items.get_mut(pos).and_then(|v| v.as_object_mut()) {
                        obj.insert("aggregatedOutput".into(), json!(out));
                        if let Some(c) = code {
                            obj.insert("exitCode".into(), json!(c));
                            if c != 0 {
                                obj.insert("status".into(), json!("failed"));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    items
}

#[tauri::command]
pub async fn regenerate(app: AppHandle, state: State<'_, AppState>, args: SendArgs) -> Result<(), String> {
    let (personality, kali_mode, custom, docker_mode) = {
        let s = state.settings.lock().await;
        s.export_env();
        (s.personality.clone(), s.kali_mode, s.custom_instructions.clone(), s.runtime == "docker")
    };
    let overrides = turn_overrides_from(&state, &args, personality.as_deref(), kali_mode, custom.as_deref(), docker_mode);
    let turns = args.turns.unwrap_or(1).max(1);
    let harness = ensure_harness(&state, &app).await?;
    harness
        .regenerate(&args.thread_id, args.text, overrides, args.images, turns)
        .await
        .map_err(|e| e.to_string())
}

#[derive(Deserialize)]
pub struct ApprovalArgs {
    pub token: String,
    pub approve: bool,
}

#[tauri::command]
pub async fn submit_approval(app: AppHandle, state: State<'_, AppState>, args: ApprovalArgs) -> Result<(), String> {
    let harness = ensure_harness(&state, &app).await?;
    harness
        .resolve_approval(&args.token, args.approve)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn pick_directory(app: AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_folder(move |path| {
        let _ = tx.send(path.map(|p| p.to_string()));
    });
    rx.await.map_err(|e| e.to_string())
}

// ---- bundled Docker runtime (codex + ocx + tools, so users need only Docker) ----

const RUNTIME_IMAGE: &str = "hacksor-runtime:latest";
const RUNTIME_CONTAINER: &str = "hacksor-runtime";
const RUNTIME_DOCKERFILE: &str = include_str!("../../docker/Dockerfile");
/// Prebuilt image published to a public registry. When set (and reachable), the
/// runtime is PULLED (fast, streamed progress) instead of built locally. Kept in
/// sync with the app version by CI (set `HACKSOR_RUNTIME_IMAGE` at build time).
/// Empty => always build locally.
const RUNTIME_REGISTRY_IMAGE: &str = match option_env!("HACKSOR_RUNTIME_IMAGE") {
    Some(v) => v,
    None => "",
};

/// Pull the prebuilt runtime image from the registry via the Docker Engine API,
/// streaming aggregated layer-download progress to the UI on `hacksor://runtime`
/// (percent + human bytes), then tag it locally as `RUNTIME_IMAGE` so the rest of
/// the code finds it. Cross-platform: bollard talks the Docker socket on
/// Linux/macOS and the named pipe on Windows.
async fn pull_runtime_image(app: &AppHandle) -> Result<(), String> {
    let image = RUNTIME_REGISTRY_IMAGE;
    let _ = app.emit("hacksor://runtime", serde_json::json!({
        "phase": "pulling", "percent": 0, "message": "Preparing to download the runtime…"
    }));
    let app2 = app.clone();
    let mb = |b: i64| (b as f64 / 1_048_576.0).round() as i64;
    runtime::pull(image, move |cur, tot| {
        let percent = if tot > 0 { ((cur as f64 / tot as f64) * 100.0).round() as u32 } else { 0 };
        let _ = app2.emit("hacksor://runtime", serde_json::json!({
            "phase": "pulling",
            "percent": percent,
            "message": if tot > 0 { format!("Downloading runtime… {} / {} MB", mb(cur), mb(tot)) } else { "Downloading runtime…".to_string() }
        }));
    })
    .await?;
    // Tag the pulled image as the local name the runtime code expects.
    let (repo, tag) = RUNTIME_IMAGE.split_once(':').unwrap_or((RUNTIME_IMAGE, "latest"));
    runtime::tag(image, repo, tag).await?;
    let _ = app.emit("hacksor://runtime", serde_json::json!({
        "phase": "pulling", "percent": 100, "message": "Runtime downloaded."
    }));
    Ok(())
}

/// Ensure the runtime image exists locally: pull the prebuilt image with a
/// progress bar when a registry image is configured, else build it locally.
/// (Progress is emitted on `hacksor://runtime`.)
async fn provision_runtime_image(state: &State<'_, AppState>, app: &AppHandle) -> Result<(), String> {
    if !RUNTIME_REGISTRY_IMAGE.is_empty() {
        match pull_runtime_image(app).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                // Registry unreachable / image missing — fall back to a local build.
                let _ = app.emit("hacksor://runtime", serde_json::json!({
                    "phase": "building",
                    "message": format!("Pull failed ({e}); building the runtime locally (first run, a few minutes)…")
                }));
            }
        }
    } else {
        let _ = app.emit("hacksor://runtime", serde_json::json!({
            "phase": "building",
            "message": "Building runtime image (first run, a few minutes)…"
        }));
    }
    build_runtime_image(app, &state.codex_home).await.map_err(|e| format!("Runtime image build failed: {e}"))
}

async fn runtime_image_exists() -> bool {
    runtime::image_exists(RUNTIME_IMAGE).await
}

async fn runtime_container_running() -> bool {
    runtime::container_running(RUNTIME_CONTAINER).await
}

#[derive(Serialize)]
pub struct RuntimeStatus {
    pub docker: bool,
    pub image: bool,
    pub running: bool,
}

#[tauri::command]
pub async fn runtime_status() -> RuntimeStatus {
    let docker = docker_present().await;
    RuntimeStatus {
        docker,
        image: docker && runtime_image_exists().await,
        running: docker && runtime_container_running().await,
    }
}

/// Path of the user-customized Dockerfile (empty/absent ⇒ use the embedded one).
fn custom_dockerfile_path(codex_home: &std::path::Path) -> std::path::PathBuf {
    codex_home.join("runtime").join("Dockerfile.custom")
}

/// The Dockerfile that will actually be built: the user's customized copy if it
/// exists and is non-empty, else the embedded default.
fn effective_dockerfile(codex_home: &std::path::Path) -> String {
    match std::fs::read_to_string(custom_dockerfile_path(codex_home)) {
        Ok(t) if !t.trim().is_empty() => t,
        _ => RUNTIME_DOCKERFILE.to_string(),
    }
}

/// Build the runtime image from the effective Dockerfile via the Engine API,
/// streaming build log lines to the UI. Honors a user-customized Dockerfile.
async fn build_runtime_image(app: &AppHandle, codex_home: &std::path::Path) -> Result<(), String> {
    if !docker_present().await {
        return Err("Docker is not installed or not running.".into());
    }
    let dockerfile = effective_dockerfile(codex_home);
    let app2 = app.clone();
    runtime::build(&dockerfile, RUNTIME_IMAGE, move |line| {
        // Full build log, streamed line-by-line for the output viewer the user
        // can open from the build banner.
        let _ = app2.emit("hacksor://runtime-log", serde_json::json!({ "line": line }));
        // Surface the last build step so the banner isn't a dead "Building…".
        if line.starts_with("Step") || line.starts_with("#") {
            let _ = app2.emit("hacksor://runtime", serde_json::json!({
                "phase": "building", "message": format!("Building runtime… {}", line.chars().take(80).collect::<String>())
            }));
        }
    })
    .await
}

/// Build the runtime image (explicit Build button).
#[tauri::command]
pub async fn build_runtime(state: State<'_, AppState>, app: AppHandle) -> Result<(), String> {
    build_runtime_image(&app, &state.codex_home).await
}

// ---- User-editable Dockerfile + persona ----

#[derive(Serialize)]
pub struct EditableText {
    pub text: String,
    pub is_custom: bool,
}

/// The current runtime Dockerfile (customized copy or the embedded default).
#[tauri::command]
pub fn get_dockerfile(state: State<'_, AppState>) -> EditableText {
    let custom = std::fs::read_to_string(custom_dockerfile_path(&state.codex_home))
        .ok()
        .filter(|t| !t.trim().is_empty());
    EditableText {
        is_custom: custom.is_some(),
        text: custom.unwrap_or_else(|| RUNTIME_DOCKERFILE.to_string()),
    }
}

/// Save a customized Dockerfile (empty ⇒ revert to the embedded default). Does
/// NOT rebuild — the user rebuilds explicitly (it takes minutes).
#[tauri::command]
pub fn save_dockerfile(state: State<'_, AppState>, content: String) -> Result<(), String> {
    let p = custom_dockerfile_path(&state.codex_home);
    std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
    if content.trim().is_empty() {
        let _ = std::fs::remove_file(&p);
    } else {
        std::fs::write(&p, content).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The current cybersecurity-agent system prompt (customized copy or the
/// embedded default).
#[tauri::command]
pub fn get_persona(state: State<'_, AppState>) -> EditableText {
    let custom = std::fs::read_to_string(persona_file(&state.codex_home))
        .ok()
        .filter(|t| !t.trim().is_empty());
    EditableText {
        is_custom: custom.is_some(),
        text: custom.unwrap_or_else(|| state.developer_prompt.clone()),
    }
}

/// Save a customized system prompt (empty ⇒ revert to the embedded default).
/// Applies to the next turn (the persona is read per turn).
#[tauri::command]
pub fn save_persona(state: State<'_, AppState>, content: String) -> Result<(), String> {
    let p = persona_file(&state.codex_home);
    std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
    if content.trim().is_empty() {
        let _ = std::fs::remove_file(&p);
    } else {
        std::fs::write(&p, content).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Rebuild the runtime image from the (possibly customized) Dockerfile, then
/// restart the container so the new image is used. Progress on `hacksor://runtime`.
#[tauri::command]
pub async fn rebuild_runtime(state: State<'_, AppState>, app: AppHandle) -> Result<(), String> {
    let _ = app.emit("hacksor://runtime", serde_json::json!({
        "phase": "building", "message": "Rebuilding runtime image from your Dockerfile (a few minutes)…"
    }));
    build_runtime_image(&app, &state.codex_home).await.map_err(|e| format!("Rebuild failed: {e}"))?;
    // Recreate the container on the fresh image + drop the harness so it respawns.
    runtime::remove_container(RUNTIME_CONTAINER).await;
    if let Some(h) = state.harness.lock().await.take() {
        h.shutdown().await;
    }
    let _ = app.emit("hacksor://runtime", serde_json::json!({
        "phase": "ready", "message": "Runtime rebuilt. It will start on your next message."
    }));
    Ok(())
}

/// Path of the user-customized system prompt (absent ⇒ use the embedded one).
fn persona_file(codex_home: &std::path::Path) -> std::path::PathBuf {
    codex_home.join("persona.md")
}

/// The effective cybersecurity-agent system prompt: the user's customized copy
/// if present and non-empty, else the embedded default. Read per turn so edits
/// apply without an app restart.
fn effective_persona(state: &AppState) -> String {
    match std::fs::read_to_string(persona_file(&state.codex_home)) {
        Ok(t) if !t.trim().is_empty() => t,
        _ => state.developer_prompt.clone(),
    }
}

/// Provision + start the runtime so the app is ready to chat WITHOUT waiting for
/// the first message: in Docker mode pull/build the image (progress bar), start
/// the container, the proxy and ocx; in host mode just start ocx. Safe to call
/// repeatedly (idempotent). Called on app startup (when Docker is the runtime)
/// and when the user switches the runtime to Docker.
async fn ensure_runtime_ready(state: &State<'_, AppState>, app: &AppHandle) -> Result<(), String> {
    if !docker_runtime(state).await {
        return ensure_opencodex(state).await;
    }
    if !docker_present().await {
        return Err("Docker mode is selected but Docker isn't installed or running. Install/start Docker (docker.com/get-started), or switch Runtime to Host in Settings.".into());
    }
    if !runtime_image_exists().await {
        provision_runtime_image(state, app).await?;
    }
    let _ = app.emit("hacksor://runtime", serde_json::json!({ "phase": "starting", "message": "Starting runtime container…" }));
    // On-demand (default): don't pre-start the heavy proxy/browser — the agent's
    // `hacksor-proxy start` and the patched camofox-mcp client bring them up when
    // actually used, saving RAM. Always-on: pre-start both to avoid warm-up.
    let always_on = { state.settings.lock().await.services_mode == "always_on" };
    start_runtime(&state.codex_home, always_on).await?;
    if always_on {
        autostart_proxy_docker(&state.codex_home).await;
    } else {
        prepare_proxy_docker(&state.codex_home).await; // write CLI + wrapper so the agent can start it
    }
    ensure_opencodex(state).await?;
    let _ = app.emit("hacksor://runtime", serde_json::json!({ "phase": "ready", "message": "Runtime ready." }));
    Ok(())
}

/// Front-end entry point: prepare the runtime up front (startup / runtime switch)
/// so all the Docker download+build+start happens before the first message.
#[tauri::command]
pub async fn prepare_runtime(state: State<'_, AppState>, app: AppHandle) -> Result<(), String> {
    ensure_runtime_ready(&state, &app).await
}

/// Start (or reuse) the runtime container. On Linux it shares the host network
/// and runs as the host uid:gid (mounting $HOME at its real path). On macOS /
/// Windows (Docker Desktop VM) it mounts the user home and runs as root; the
/// container reaches the host-side OpenCodex proxy via `host.docker.internal`.
/// The OpenCodex proxy always runs on the HOST (managed cross-platform by the
/// app), so it is NOT started inside the container.
async fn start_runtime(codex_home: &std::path::Path, services_always_on: bool) -> Result<(), String> {
    if !docker_present().await {
        return Err("Docker is not installed.".into());
    }
    if !runtime_image_exists().await {
        return Err("Runtime image not available yet — it downloads/builds on first use.".into());
    }
    if !runtime_container_running().await {
        let (host_home, container_home) = {
            let (mount, home_env) = platform::home_mount();
            (mount.split_once(':').map(|(h, _)| h.to_string()).unwrap_or(home_env.clone()), home_env)
        };
        let spec = runtime::RunSpec {
            image: RUNTIME_IMAGE.to_string(),
            name: RUNTIME_CONTAINER.to_string(),
            host_home,
            container_home,
            linux: platform::IS_LINUX,
            tun: runtime::tun_present(),
            // Docker Desktop VM: publish the in-container ocx + intercepting-proxy
            // ports so the host app reaches them at 127.0.0.1.
            publish_ports: if platform::IS_LINUX { vec![] } else { vec![models::OPENCODEX_PORT, PROXY_PORT] },
        };
        runtime::create_and_start(&spec).await?;
        // Linux only: register the host uid/gid in the container passwd + grant
        // passwordless sudo, so codex/ocx run as the host user (no root-owned
        // files) yet can still sudo. On macOS/Windows we run as root (Docker
        // Desktop maps ownership), so this is unnecessary.
        if let Some(user) = platform::docker_exec_user() {
            let (uid, gid) = user.split_once(':').unwrap_or(("1000", "1000"));
            let setup = format!(
                "getent group {gid} >/dev/null || groupadd -g {gid} hacksor; \
                 getent passwd {uid} >/dev/null || useradd -o -u {uid} -g {gid} -d \"$HOME\" -s /bin/bash hacksor; \
                 echo '#{uid} ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/hacksor; \
                 printf '%s ALL=(ALL) NOPASSWD:ALL\\n' \"$(getent passwd {uid} | cut -d: -f1)\" >> /etc/sudoers.d/hacksor; \
                 chmod 440 /etc/sudoers.d/hacksor"
            );
            let _ = runtime::exec_output(RUNTIME_CONTAINER, vec!["bash".into(), "-lc".into(), setup], Some("0".into()), None).await;
        }
    }
    let _ = codex_home; // the home mount already covers codex-home + working dirs
    // Refresh vuln intel in the background so each session has fresh CVE data.
    let _ = runtime::exec_detached(
        RUNTIME_CONTAINER,
        vec!["sh".into(), "-c".into(), "nuclei -update-templates -silent >/dev/null 2>&1; searchsploit -u >/dev/null 2>&1".into()],
        platform::docker_exec_user(),
    )
    .await;
    // Start the Camoufox stealth-browser server (the camofox MCP connects to it
    // on 127.0.0.1:9377). Use camofox-browser's OWN daemon (`server start
    // --background`): it self-spawns Xvfb and detaches, so it survives the exec
    // shell exiting (the old `xvfb-run -a npx …` form died with its parent). We
    // run the image's installed binary — NOT `npx @latest` — so the build-time
    // disable_coop patch (needed to click cross-origin Turnstile iframes) sticks.
    // GUARD: probe the /health endpoint, NOT `pgrep -f camofox-browser`. Under
    // `sh -c "<string>"` the shell's own argv contains "camofox-browser", so
    // `pgrep -f camofox-browser` matches itself and the `||` would skip the start
    // forever. The health check is immune to that and also restarts a hung/dead
    // daemon. Idempotent + self-healing (re-run on every container start).
    // MEMORY: camofox-browser pools browser contexts up to CAMOFOX_MAX_SESSIONS
    // (default 50!) and keeps idle ones for CAMOFOX_IDLE_TIMEOUT_MS (default 30m).
    // Each context is a live Firefox window → lots of RAM. Hacksor is single-user,
    // so cap the pool to 3 and evict idle contexts after 5 min to keep the browser
    // footprint small (LRU eviction frees the memory; the base daemon stays up).
    // On-demand (default): DON'T pre-start the heavy Firefox daemon — the patched
    // camofox-mcp client lazy-starts it (with these same RAM caps) on the first
    // browser tool call. Only pre-start it in always-on mode to avoid warm-up.
    // GUARD: probe /health, NOT `pgrep -f camofox-browser` — under `sh -c "…"` the
    // shell's own argv contains "camofox-browser", so pgrep would match itself and
    // the `||` would skip the start forever. Health check is immune + self-healing.
    if services_always_on {
        let _ = runtime::exec_detached(
            RUNTIME_CONTAINER,
            vec![
                "sh".into(),
                "-c".into(),
                "curl -fsS -m2 http://127.0.0.1:9377/health >/dev/null 2>&1 || \
                 CAMOFOX_HOST=127.0.0.1 CAMOFOX_AUTH_MODE=disabled \
                 CAMOFOX_MAX_SESSIONS=3 CAMOFOX_IDLE_TIMEOUT_MS=300000 \
                 camofox-browser server start --port 9377 --background >>/tmp/hacksor-camofox.log 2>&1"
                    .into(),
            ],
            platform::docker_exec_user(),
        )
        .await;
    }
    Ok(())
}

/// Update vulnerability intelligence: nuclei templates + Exploit-DB mirror.
/// Runs inside the container in Docker mode, else on the host if tools exist.
#[tauri::command]
pub async fn refresh_intel(state: State<'_, AppState>) -> Result<String, String> {
    let docker = { state.settings.lock().await.runtime == "docker" };
    let script = "nuclei -update-templates -silent 2>&1 | tail -2; searchsploit -u 2>&1 | tail -2";
    if docker {
        if !runtime_container_running().await {
            return Err("Runtime container is not running.".to_string());
        }
        let (out, _) = runtime::exec_output(
            RUNTIME_CONTAINER,
            vec!["sh".into(), "-c".into(), script.into()],
            platform::docker_exec_user(),
            None,
        )
        .await?;
        Ok(out.trim().to_string())
    } else {
        if which_bin("nuclei").is_none() && which_bin("searchsploit").is_none() {
            return Err("nuclei/searchsploit not on host. Use Docker runtime (they're bundled there).".into());
        }
        let mut out = String::new();
        if which_bin("nuclei").is_some() {
            if let Ok(o) = tokio::process::Command::new("nuclei").args(["-update-templates", "-silent"]).output().await {
                out.push_str(String::from_utf8_lossy(&o.stderr).trim());
                out.push('\n');
            }
        }
        if which_bin("searchsploit").is_some() {
            if let Ok(o) = tokio::process::Command::new("searchsploit").arg("-u").output().await {
                out.push_str(String::from_utf8_lossy(&o.stdout).trim());
            }
        }
        Ok(out.trim().to_string())
    }
}

// ---- HTTP intercepting proxy (Burp-style toolkit on mitmproxy) ----

// The `hacksor-proxy` CLI: one Python file that is BOTH the mitmproxy capture
// addon (loaded via `mitmdump -s …`, logging flows + applying scope/match-replace
// rules) and the agent's terminal Burp (history/show/repeat/intruder/decode/
// compare/scope/match-replace). Bundled from the repo so there's one source of
// truth; written to codex_home/proxy at runtime so both the host UI and the
// container share it over the bind mount.
const PROXY_CLI: &str = include_str!("../../docker/hacksor-proxy");

/// Status of the runtime's background services, surfaced read-only in Settings
/// (the intercepting proxy + the Camoufox stealth browser). Both autostart in
/// the container; there are no controls, just visibility.
#[derive(Serialize)]
pub struct ServicesStatus {
    pub docker: bool,
    pub proxy_running: bool,
    pub proxy_port: u16,
    pub browser_running: bool,
    pub browser_port: u16,
    pub ca_path: String,
    /// "on_demand" or "always_on" — how the services are brought up.
    pub mode: String,
}

const PROXY_PORT: u16 = 8888;
const BROWSER_PORT: u16 = 9377;

/// Proxy files under codex_home/proxy: (CLI script, flow history, rules).
fn proxy_paths(codex_home: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let dir = codex_home.join("proxy");
    (dir.join("hacksor-proxy"), dir.join("flows.jsonl"), dir.join("rules.json"))
}

/// Write the bundled `hacksor-proxy` CLI/addon (executable) + ensure the flow
/// log exists. Returns the (script, log, rules) paths.
fn write_proxy_cli(codex_home: &std::path::Path) -> std::io::Result<(std::path::PathBuf, std::path::PathBuf, std::path::PathBuf)> {
    let (script, log, rules) = proxy_paths(codex_home);
    std::fs::create_dir_all(script.parent().unwrap())?;
    std::fs::write(&script, PROXY_CLI)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755));
    }
    Ok((script, log, rules))
}

/// Always-run prep: write the hacksor-proxy CLI, delete any stale `addon.py` from
/// an older build, and install the `hacksor-proxy` PATH wrapper — WITHOUT starting
/// mitmdump. This makes the CLI available so the agent can `hacksor-proxy start`
/// on demand (on-demand mode), and is a prerequisite for the actual start.
async fn prepare_proxy_docker(codex_home: &std::path::Path) {
    let (script, log, _rules) = match write_proxy_cli(codex_home) {
        Ok(p) => p,
        Err(_) => return,
    };
    // The `[m]itmdump` bracket trick stops pkill from matching (and killing) this
    // very `sh -c` wrapper, whose own argv contains the literal "mitmdump".
    let proxy_dir = platform::to_container_path(script.parent().unwrap_or(&script));
    let _ = runtime::exec_output(
        RUNTIME_CONTAINER,
        vec!["sh".into(), "-c".into(),
             format!("rm -rf '{proxy_dir}/addon.py' '{proxy_dir}/__pycache__' >/dev/null 2>&1 || true")],
        Some("0".into()),
        None,
    )
    .await;
    if !log.exists() {
        let _ = std::fs::write(&log, "");
    }
    install_proxy_wrapper(codex_home).await; // `hacksor-proxy` on PATH for the agent
}

/// Best-effort: start mitmdump inside the runtime container if not already up
/// (used in always-on mode; on-demand relies on the agent's `hacksor-proxy start`).
async fn autostart_proxy_docker(codex_home: &std::path::Path) {
    if proxy_running_docker().await {
        return;
    }
    prepare_proxy_docker(codex_home).await;
    let (script, log, rules) = proxy_paths(codex_home);
    // Kill any stale mitmdump (e.g. one loading the old logging-only addon.py):
    // it wouldn't match proxy_running_docker() but still holds port 8888.
    let _ = runtime::exec_output(
        RUNTIME_CONTAINER,
        vec!["sh".into(), "-c".into(), "pkill -f '[m]itmdump' >/dev/null 2>&1 || true".into()],
        Some("0".into()),
        None,
    )
    .await;
    let _ = runtime::exec_detached(RUNTIME_CONTAINER, proxy_start_cmd(&script, &log, &rules), platform::docker_exec_user()).await;
    // Trust the mitmproxy CA in the container's system store (best-effort) so
    // tools routed through the proxy don't choke on the intercept cert. mitmdump
    // writes the CA to $HOME/.mitmproxy on first start; give it a moment.
    let _ = runtime::exec_output(
        RUNTIME_CONTAINER,
        vec!["sh".into(), "-c".into(),
             "for i in 1 2 3 4 5 6; do [ -f \"$HOME/.mitmproxy/mitmproxy-ca-cert.pem\" ] && break; sleep 0.5; done; \
              if [ -f \"$HOME/.mitmproxy/mitmproxy-ca-cert.pem\" ]; then \
                cp \"$HOME/.mitmproxy/mitmproxy-ca-cert.pem\" /usr/local/share/ca-certificates/mitmproxy-hacksor.crt 2>/dev/null && \
                update-ca-certificates >/dev/null 2>&1 || true; fi".into()],
        Some("0".into()),
        None,
    )
    .await;
}

/// The command that starts mitmdump inside the runtime container, loading the
/// hacksor-proxy script as its addon. Script/log/rules paths are translated to
/// their container paths (identity on Linux/macOS; remapped under the Windows
/// home mount).
fn proxy_start_cmd(script: &std::path::Path, log: &std::path::Path, rules: &std::path::Path) -> Vec<String> {
    vec![
        "env".into(),
        format!("HACKSOR_PROXY_LOG={}", platform::to_container_path(log)),
        format!("HACKSOR_PROXY_RULES={}", platform::to_container_path(rules)),
        format!("HACKSOR_PROXY_PORT={}", PROXY_PORT),
        "mitmdump".into(),
        "-q".into(),
        "--listen-port".into(),
        PROXY_PORT.to_string(),
        // MEMORY: stream response/request bodies larger than 1 MB straight through
        // instead of buffering the whole thing in RAM (big downloads/uploads no
        // longer balloon mitmproxy). Our addon still logs headers + clipped text.
        "--set".into(),
        "stream_large_bodies=1m".into(),
        "-s".into(),
        platform::to_container_path(script),
    ]
}

/// Drop a `hacksor-proxy` wrapper on the container PATH so the agent can invoke
/// the Burp-style CLI by name (with the history/rules/port env baked in). The
/// script lives on the bind-mounted codex_home; the wrapper just injects env
/// and runs it through python3. Idempotent.
async fn install_proxy_wrapper(codex_home: &std::path::Path) {
    let (script, log, rules) = proxy_paths(codex_home);
    let wrapper = format!(
        "#!/bin/sh\nexec env HACKSOR_PROXY_LOG='{log}' HACKSOR_PROXY_RULES='{rules}' HACKSOR_PROXY_PORT={port} python3 '{script}' \"$@\"\n",
        log = platform::to_container_path(&log),
        rules = platform::to_container_path(&rules),
        port = PROXY_PORT,
        script = platform::to_container_path(&script),
    );
    let cmd = format!(
        "cat > /usr/local/bin/hacksor-proxy <<'HXEOF'\n{wrapper}HXEOF\nchmod +x /usr/local/bin/hacksor-proxy"
    );
    let _ = runtime::exec_output(
        RUNTIME_CONTAINER,
        vec!["sh".into(), "-c".into(), cmd],
        Some("0".into()),
        None,
    )
    .await;
}

/// Is OUR proxy running inside the runtime container? Matches the hacksor-proxy
/// script specifically — a stale `mitmdump -s …/addon.py` from an older build
/// must NOT count as running, or autostart would return early forever and never
/// migrate to the current Burp-style addon.
async fn proxy_running_docker() -> bool {
    if !runtime_container_running().await {
        return false;
    }
    match runtime::exec_output(RUNTIME_CONTAINER, vec!["pgrep".into(), "-f".into(), "mitmdump.*proxy/hacksor-proxy".into()], None, None).await {
        Ok((_, code)) => code == 0,
        Err(_) => false,
    }
}

/// Is the Camoufox stealth browser server running inside the runtime container?
async fn browser_running_docker() -> bool {
    if !runtime_container_running().await {
        return false;
    }
    match runtime::exec_output(RUNTIME_CONTAINER, vec!["pgrep".into(), "-f".into(), "camofox-browser".into()], None, None).await {
        Ok((_, code)) => code == 0,
        Err(_) => false,
    }
}

/// Read-only status of the runtime's background services for the Settings panel.
/// Both the proxy and the browser autostart inside the container; this just
/// reports whether they're up (Docker runtime only — they're bundled there).
#[tauri::command]
pub async fn services_status(state: State<'_, AppState>) -> Result<ServicesStatus, String> {
    let (docker, mode) = {
        let s = state.settings.lock().await;
        (s.runtime == "docker", s.services_mode.clone())
    };
    let (proxy_running, browser_running) = if docker {
        (proxy_running_docker().await, browser_running_docker().await)
    } else {
        (false, false)
    };
    // The mitmproxy CA lands at $HOME/.mitmproxy (the container mounts $HOME, so
    // it's the same path on host and in the container).
    let ca = dirs::home_dir()
        .map(|h| h.join(".mitmproxy/mitmproxy-ca-cert.pem").to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(ServicesStatus {
        docker,
        proxy_running,
        proxy_port: PROXY_PORT,
        browser_running,
        browser_port: BROWSER_PORT,
        ca_path: ca,
        mode,
    })
}

/// Best-effort teardown of the runtime container. Called on app exit (a sync
/// context) so quitting never leaves an orphaned `hacksor-runtime` behind.
pub fn cleanup_runtime() {
    tauri::async_runtime::block_on(async {
        if runtime::docker_present().await {
            runtime::remove_container(RUNTIME_CONTAINER).await;
            runtime::remove_container(KALI_CONTAINER).await;
        }
    });
}

#[tauri::command]
pub async fn stop_runtime() -> Result<(), String> {
    runtime::remove_container(RUNTIME_CONTAINER).await;
    Ok(())
}

// ---- Kali container run-mode ----

const KALI_CONTAINER: &str = "hacksor-kali";

#[derive(Serialize)]
pub struct KaliStatus {
    pub docker: bool,   // docker engine reachable
    pub running: bool,  // the hacksor-kali container is up
}

async fn docker_present() -> bool {
    runtime::docker_present().await
}

#[tauri::command]
pub async fn kali_status() -> KaliStatus {
    let docker = docker_present().await;
    let running = docker && runtime::container_running(KALI_CONTAINER).await;
    KaliStatus { docker, running }
}

/// Start (or reuse) a persistent Kali container with the working dir mounted at
/// the same path, so the agent can exec tools against local files.
#[tauri::command]
pub async fn start_kali(working_dir: String) -> Result<String, String> {
    if !docker_present().await {
        return Err("Docker is not installed or not running.".into());
    }
    if runtime::container_running(KALI_CONTAINER).await {
        return Ok("already running".into());
    }
    let wd = if working_dir.trim().is_empty() { "/root".to_string() } else { working_dir };
    let spec = runtime::RunSpec {
        image: "kalilinux/kali-rolling".into(),
        name: KALI_CONTAINER.into(),
        host_home: wd.clone(),
        container_home: wd,
        linux: platform::IS_LINUX,
        tun: runtime::tun_present(),
        publish_ports: vec![],
    };
    runtime::create_and_start(&spec).await.map(|_| "started".to_string())
}

#[tauri::command]
pub async fn stop_kali() -> Result<(), String> {
    runtime::remove_container(KALI_CONTAINER).await;
    Ok(())
}

// ---- findings notes (shared with the agent via codex_home/notes) ----

fn notes_dir(state: &AppState) -> PathBuf {
    state.codex_home.join("notes")
}

#[derive(Serialize)]
pub struct NoteFile {
    pub name: String,
    pub content: String,
}

#[tauri::command]
pub fn read_notes(state: State<'_, AppState>) -> Result<Vec<NoteFile>, String> {
    let dir = notes_dir(&state);
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("md") {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("note.md").to_string();
                let content = std::fs::read_to_string(&p).unwrap_or_default();
                out.push(NoteFile { name, content });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

#[tauri::command]
pub fn write_note(state: State<'_, AppState>, name: String, content: String) -> Result<(), String> {
    let dir = notes_dir(&state);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let safe: String = name.chars().filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ')).collect();
    let file = if safe.trim().is_empty() { "note".to_string() } else { safe.trim().to_string() };
    let file = if file.ends_with(".md") { file } else { format!("{file}.md") };
    std::fs::write(dir.join(file), content).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_note(state: State<'_, AppState>, name: String) -> Result<(), String> {
    let p = notes_dir(&state).join(&name);
    if p.exists() { std::fs::remove_file(p).map_err(|e| e.to_string())?; }
    Ok(())
}

/// Save arbitrary text to a file the user chooses (used for exporting a chat).
#[tauri::command]
pub async fn save_text_dialog(app: AppHandle, default_name: String, content: String) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_file_name(&default_name)
        .save_file(move |path| { let _ = tx.send(path); });
    let path = rx.await.map_err(|e| e.to_string())?;
    match path {
        Some(fp) => {
            let s = fp.to_string();
            std::fs::write(&s, content).map_err(|e| e.to_string())?;
            Ok(Some(s))
        }
        None => Ok(None),
    }
}

#[tauri::command]
pub async fn pick_files(app: AppHandle) -> Result<Vec<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_files(move |paths| {
        let list = paths
            .map(|v| v.into_iter().map(|p| p.to_string()).collect::<Vec<_>>())
            .unwrap_or_default();
        let _ = tx.send(list);
    });
    rx.await.map_err(|e| e.to_string())
}

async fn ensure_harness(state: &State<'_, AppState>, app: &AppHandle) -> Result<Arc<Harness>, String> {
    let docker_mode = { state.settings.lock().await.runtime == "docker" };
    // Provision + start the runtime and the OpenCodex proxy if not already done
    // (normally the frontend calls prepare_runtime up front so this is a no-op).
    // Idempotent; a hard failure (e.g. Docker not running) surfaces here.
    ensure_runtime_ready(state, app).await?;

    let mut guard = state.harness.lock().await;
    if let Some(h) = guard.as_ref() {
        // Reuse only if alive AND its launch mode matches the current runtime
        // setting; otherwise (dead, or runtime was switched host<->docker) drop
        // and respawn in the correct mode.
        if h.is_alive() && h.is_docker == docker_mode {
            return Ok(Arc::clone(h));
        }
        *guard = None;
    }
    let launch = if docker_mode {
        crate::harness::LaunchMode::Docker { container: RUNTIME_CONTAINER.to_string(), user: platform::docker_exec_user() }
    } else {
        crate::harness::LaunchMode::Host
    };
    let harness = Harness::new(state.codex_home.clone(), app.clone(), launch)
        .await
        .map_err(|e| e.to_string())?;
    *guard = Some(Arc::clone(&harness));
    Ok(harness)
}

/// Assemble developer instructions: a runtime header (date, model, mode,
/// personality) plus the ported HackerAI persona.
fn build_developer_prompt(
    base: &str,
    env_note: &str,
    mode: &str,
    model_slug: &str,
    personality: Option<&str>,
    role: &str,
    kali_cwd: Option<&str>,
    custom: Option<&str>,
) -> String {
    let date = chrono::Local::now().format("%A, %B %-d, %Y");
    let mode_line = if mode == "ask" {
        "You are in ASK MODE. Advise and explain; the environment is read-only, so do not attempt to run destructive commands."
    } else {
        "You are in AGENT MODE. Use your tools to complete the task end to end."
    };
    let persona = personality_section(personality);
    let custom_section = match custom.map(str::trim).filter(|c| !c.is_empty()) {
        Some(c) => format!("\n\n<custom_instructions>\nThe user provided these standing instructions; follow them unless they conflict with your security mandate:\n{c}\n</custom_instructions>"),
        None => String::new(),
    };
    let role_section = role_section(role);
    let kali_section = match kali_cwd {
        Some(cwd) => format!(
            "\n\n<kali_mode>\nKALI CONTAINER MODE IS ACTIVE. Run security tools inside the container named `hacksor-kali` via `docker exec hacksor-kali <tool> ...` (the container has the Kali toolchain). The working directory {cwd} is mounted at the same path inside the container, so file paths match on both sides. Use the host shell only for non-tool operations (reading/writing files, orchestration).\n</kali_mode>"
        ),
        None => String::new(),
    };
    let role_prefixed = if role_section.is_empty() { String::new() } else { format!("\n\n{}", role_section.trim_end()) };
    // Put ALL the stable content (persona, personality, custom, role, kali, env)
    // FIRST as a byte-identical prefix, and the VOLATILE runtime line (date +
    // model + mode) LAST. The volatile bits used to lead the prompt, which
    // changed the cache prefix every time Auto switched model → cache miss and a
    // full re-bill. Keeping them at the end lets providers cache the big persona
    // prefix across turns (Anthropic/DeepSeek/OpenAI prompt caching).
    format!(
        "{base}{persona}{custom_section}{role_prefixed}{kali_section}\n\n{env_note}\n\n<runtime_context>\nThe current date is {date}. You are running on the model {model_slug}. {mode_line}\n</runtime_context>"
    )
}

/// Security sub-agent role framing, ported from HackerAI's subagent profiles.
fn role_section(role: &str) -> String {
    let body = match role {
        "task" => "<subagent_role>\nYou are a focused security SUB-AGENT executing ONE bounded, authorized security task delegated to you. Stay strictly within the task's scope and success criteria. Do the work with your tools, gather concrete evidence, and finish with a clear result: what you did, what you found (with evidence and reproduction), and whether each success criterion was met. Do not expand scope, start unrelated work, or delegate further.\n</subagent_role>",
        "validate" => "<subagent_role>\nYou are an INDEPENDENT security VALIDATION sub-agent. Your only job is to reproduce-or-falsify ONE specific vulnerability candidate. Do NOT trust prior claims or summaries — verify from scratch with your own evidence. Conclude explicitly with CONFIRMED (include reproduction steps and demonstrated impact) or NOT CONFIRMED (include the counter-evidence and why it fails). Calibrate severity only to what you actually demonstrated; record an unresolved candidate rather than silently passing.\n</subagent_role>",
        _ => return String::new(),
    };
    format!("{body}\n\n")
}

/// HackerAI personality presets, ported from `lib/system-prompt/personality.ts`.
fn personality_section(personality: Option<&str>) -> String {
    let body = match personality {
        Some("cynic") => "Be a sharp, skeptical operator. Challenge weak assumptions, point out flaws directly, and do not sugarcoat. Stay useful, not cruel.",
        Some("robot") => "Be terse and mechanical. Minimal pleasantries, maximal signal. State facts and results plainly.",
        Some("nerd") => "Bring depth and curiosity. Explain the why behind techniques, note interesting edge cases, and cite the mechanism.",
        Some("mentor") => "Be encouraging and instructive. Explain your reasoning so the user learns, and suggest what to try next.",
        _ => return String::new(),
    };
    format!("\n\n<personality>\n{body}\n</personality>")
}

#[cfg(test)]
mod transcript_tests {
    use super::*;

    #[test]
    fn parse_exec_output_extracts_stdout_and_code() {
        let raw = "Chunk ID: ad27e1\nWall time: 0.0 seconds\nProcess exited with code 0\nOriginal token count: 2\nOutput:\nbishop\n";
        let (out, code) = parse_exec_output(raw);
        assert_eq!(out, "bishop\n");
        assert_eq!(code, Some(0));
    }

    #[test]
    fn parse_rollout_recovers_reasoning_commands_and_messages() {
        // Minimal rollout mirroring codex's on-disk shape (payload-wrapped).
        let lines = [
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"persona"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n cwd </environment_context>"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"run hostname"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"reasoning","content":[{"type":"reasoning_text","text":"I will run it."}]}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"exec_command:0","arguments":"{\"cmd\":\"hostname\"}"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"exec_command:0","output":"Chunk ID: x\nProcess exited with code 0\nOutput:\nbox-01\n"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hostname is box-01"}]}}"#,
        ];
        let items = parse_rollout(&lines.join("\n"));
        let types: Vec<&str> = items.iter().map(|i| i["type"].as_str().unwrap()).collect();
        // developer + the <environment_context> wrapper are dropped.
        assert_eq!(types, ["userMessage", "reasoning", "commandExecution", "agentMessage"]);
        assert_eq!(items[0]["content"][0]["text"], "run hostname");
        assert_eq!(items[1]["content"][0], "I will run it.");
        assert_eq!(items[2]["command"], "hostname");
        assert_eq!(items[2]["aggregatedOutput"], "box-01\n");
        assert_eq!(items[2]["exitCode"], 0);
        assert_eq!(items[3]["text"], "hostname is box-01");
    }

    #[test]
    fn carryover_keeps_messages_and_commands_and_bounds_size() {
        use serde_json::json;
        let items = vec![
            json!({"type":"userMessage","content":[{"text":"scan 10.0.0.5"}]}),
            json!({"type":"reasoning","content":["thinking"]}),
            json!({"type":"commandExecution","command":"nmap -sV 10.0.0.5","aggregatedOutput":"22/tcp open ssh\n80/tcp open http\n","exitCode":0}),
            json!({"type":"agentMessage","text":"Found SSH and HTTP."}),
        ];
        let out = carryover_from_items(&items, 12_000);
        assert!(out.contains("<conversation_carryover>"));
        assert!(out.contains("User: scan 10.0.0.5"));
        assert!(out.contains("$ nmap -sV 10.0.0.5"));
        assert!(out.contains("22/tcp open ssh"));
        assert!(out.contains("Assistant: Found SSH and HTTP."));
        assert!(!out.contains("thinking")); // reasoning dropped

        // Tiny cap keeps only the most-recent line and flags truncation.
        let small = carryover_from_items(&items, 40);
        assert!(small.contains("Assistant: Found SSH and HTTP."));
        assert!(small.contains("RECENT part"));
        assert!(!small.contains("User: scan 10.0.0.5"));

        assert!(carryover_from_items(&[], 12_000).is_empty());
    }

    #[test]
    fn provider_from_rollout_reads_session_meta() {
        let text = concat!(
            r#"{"timestamp":"t","type":"session_meta","payload":{"id":"x","model_provider":"vercel","cwd":"/tmp"}}"#, "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}"#,
        );
        assert_eq!(provider_from_rollout(text).as_deref(), Some("vercel"));
        // No session_meta → None.
        assert_eq!(provider_from_rollout(r#"{"type":"response_item","payload":{}}"#), None);
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;

    #[test]
    fn hard_deny_blocks_catastrophic_commands() {
        assert!(hard_deny("sudo rm -rf / --no-preserve-root").is_some());
        assert!(hard_deny("mkfs.ext4 /dev/sda1").is_some());
        assert!(hard_deny(":(){ :|:& };:").is_some());
        // Normal pentest commands are NOT hard-denied (the model reviews them).
        assert!(hard_deny("nmap -sV 10.0.0.5").is_none());
        assert!(hard_deny("sqlmap -u http://target/?id=1 --batch").is_none());
        assert!(hard_deny("rm -rf ./scan-output").is_none());
    }

    #[test]
    fn parse_review_defaults_to_ask_on_garbage() {
        assert_eq!(parse_review("not json").decision, "ask");
        assert_eq!(parse_review(r#"{"decision":"approve","risk":"none","reason":"recon"}"#).decision, "approve");
        assert_eq!(parse_review(r#"{"decision":"nonsense"}"#).decision, "ask");
    }
}
