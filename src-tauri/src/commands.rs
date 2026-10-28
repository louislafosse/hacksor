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

/// Push every stored provider key into OpenCodex as an upstream provider (they
/// all route through the proxy). Idempotent `provider add --force`; a removed
/// key removes the upstream. Runs against host ocx or the container's ocx per
/// mode. Returns true if anything was (re)configured.
async fn configure_ocx_upstreams(state: &AppState) -> bool {
    let docker = docker_runtime(state).await;
    let keys: Vec<(crate::models::Provider, &'static str, Option<String>)> = {
        let s = state.settings.lock().await;
        crate::models::Provider::ALL
            .iter()
            .filter_map(|p| p.ocx_upstream().map(|u| (*p, u, s.key_for(*p).map(String::from))))
            .collect()
    };
    let mut changed = false;
    for (provider, upstream, key) in keys {
        match key {
            Some(k) if !k.is_empty() => {
                let mut args = vec!["provider", "add", upstream];
                // Custom (non-registry) upstreams need their adapter + base URL.
                args.extend_from_slice(provider.ocx_custom_args());
                args.extend_from_slice(&["--api-key", &k, "--force"]);
                if run_ocx(&args, None, docker).await.is_ok() {
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
/// Is `pid` an OpenCodex process on THIS machine? Linux-only: host networking —
/// the thing that lets a host proxy own the container's port — is Linux-only
/// here, so that is the only place we need to tell the two apart.
/// Is this HOST pid really an OpenCodex proxy?
///
/// The check is mandatory before signalling the pid from `~/.opencodex/ocx.pid`:
/// the container bind-mounts $HOME, so the in-container proxy writes its OWN
/// namespaced pid (a low number like 202) into the host's file. Signalling that
/// number on the host hits an unrelated process — the user's, or the system's.
/// Unknown pid or unknown platform means NOT ours, so nothing is killed.
fn pid_is_opencodex(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    platform::process_cmdline(pid).map(cmdline_is_opencodex).unwrap_or(false)
}

/// Does this command line belong to an OpenCodex proxy? ocx runs as a bun
/// process pointed at its own package, so the package path is the reliable
/// marker — the argv[0] is bun, not ocx.
fn cmdline_is_opencodex(cmdline: String) -> bool {
    cmdline.contains("opencodex") || cmdline.contains("/ocx")
}

/// Stop an OpenCodex proxy running on the HOST, via the pid ocx records in
/// `~/.opencodex/ocx.pid`. Needed in Docker mode too: under host networking the
/// container shares the host's loopback, so the port can be held by a host `ocx`
/// that an in-container `pkill` will never see.
///
/// The pid is ALWAYS verified first. This used to skip the check in host mode on
/// the grounds that the pid file "is ours there" — but the file is shared with
/// the container, so after one Docker-mode run it holds a container pid, and the
/// host-mode path then signalled that number blind.
fn kill_host_opencodex() {
    let Some(home) = dirs::home_dir() else { return };
    let Ok(raw) = std::fs::read_to_string(home.join(".opencodex/ocx.pid")) else { return };
    let Ok(pid) = raw.trim().parse::<i64>() else { return };
    if !pid_is_opencodex(pid) {
        return;
    }
    platform::kill_pid(pid);
}

async fn restart_opencodex(state: &AppState) {
    if docker_runtime(state).await {
        // In-container ocx: kill it; ensure_opencodex restarts it after config.
        if runtime_container_running().await {
            let _ = runtime::exec_output(RUNTIME_CONTAINER, vec!["pkill".into(), "-f".into(), "opencodex".into()], None, None).await;
        }
        // The container has its own PID namespace, so the pkill above cannot
        // reach a proxy running on the host — and under host networking that
        // proxy is the one holding the port. Without this, a stale host ocx
        // keeps serving its startup-time provider config and every key change
        // is silently ignored (ocx only reads providers at startup).
        kill_host_opencodex();
    } else {
        if let Some(mut child) = state.opencodex.lock().await.take() {
            let _ = child.start_kill();
        }
        kill_host_opencodex();
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
        let (narrow, work) = {
            let s = state.settings.lock().await;
            (s.limit_runtime_mount, s.working_dir.clone())
        };
        start_runtime(&state.codex_home, false, narrow, &work).await?;
        configure_ocx_upstreams(state).await;
        // Start ocx detached, keeping its output so a crash can be reported
        // instead of surfacing only as "didn't become ready" (ocx exits with a
        // stack trace for things like an unreadable ~/.codex/config.toml). `exec`
        // replaces the shell, so this is still the exec's own single process —
        // a backgrounded `&` under a shell that exits would SIGHUP the proxy.
        let _ = runtime::exec_detached(
            RUNTIME_CONTAINER,
            vec![
                "sh".into(),
                "-c".into(),
                format!(
                    "mkdir -p {home} && export CODEX_HOME={home}; exec ocx start --port {port} > {OCX_START_LOG} 2>&1",
                    home = ocx_codex_home(true),
                    port = models::OPENCODEX_PORT
                ),
            ],
            exec_user(),
        )
        .await;
        // Without host networking (Docker Desktop VM, or rootless Docker): ocx binds
        // only to 127.0.0.1 inside the container, so the published host port (which
        // forwards to the container's external interface) can't reach it and the
        // proxy never looks "ready". Bridge the container's own IP to ocx's loopback
        // with socat so both the published port and in-container clients reach it.
        // With host networking the container shares the host's 127.0.0.1 already.
        if !rootful_linux() {
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
                exec_user(),
            )
            .await;
        }
        for _ in 0..24 {
            if opencodex_up().await {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        // Include what ocx actually printed: a bare "didn't become ready" hides
        // real, fixable causes (e.g. EACCES on a root-owned ~/.codex/config.toml).
        let log = runtime::exec_output(
            RUNTIME_CONTAINER,
            vec!["tail".into(), "-n".into(), "25".into(), OCX_START_LOG.into()],
            exec_user(),
            None,
        )
        .await
        .map(|(out, _)| out)
        .unwrap_or_default();
        let detail = log.trim();
        // A leftover ocx from a previous run refuses to start a new one ("Proxy
        // already running… Use 'ocx stop' first") — and if that old process is
        // not actually reachable, nothing recovers on its own: the model list
        // never loads and the composer sits on a placeholder until the user
        // works out what to do. Clear it and try once more.
        if detail.to_lowercase().contains("already running") {
            let _ = runtime::exec_output(
                RUNTIME_CONTAINER,
                vec!["sh".into(), "-c".into(),
                     "ocx stop >/dev/null 2>&1; pkill -f 'ocx start' >/dev/null 2>&1; true".into()],
                exec_user(),
                None,
            )
            .await;
            let _ = runtime::exec_detached(
                RUNTIME_CONTAINER,
                vec![
                    "sh".into(),
                    "-c".into(),
                    format!(
                        "mkdir -p {home} && export CODEX_HOME={home}; exec ocx start --port {port} > {OCX_START_LOG} 2>&1",
                        home = ocx_codex_home(true),
                        port = models::OPENCODEX_PORT
                    ),
                ],
                exec_user(),
            )
            .await;
            for _ in 0..24 {
                if opencodex_up().await {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
        if detail.is_empty() {
            return Err("OpenCodex proxy in the runtime container didn't become ready.".into());
        }
        return Err(format!(
            "OpenCodex proxy in the runtime container didn't become ready:\n{}",
            truncate(detail, 1200)
        ));
    }

    // Host mode: configure + spawn the host ocx.
    configure_ocx_upstreams(state).await;
    let bin = which_bin("ocx").ok_or_else(|| {
        "OpenCodex is not installed on the host. Switch Runtime to Docker (it's bundled there), or run `npm i -g @bitkyc08/opencodex`.".to_string()
    })?;
    let child = tokio::process::Command::new(bin)
        .env("CODEX_HOME", ocx_codex_home(false))
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
    // ocx serves its provider dashboard as a web page at the proxy root. Open that
    // in the browser instead of running `ocx gui`: it works on every platform and
    // in Docker mode (where ocx is only in the container — the port is published/
    // bridged to the host), with no host ocx install and no display needed.
    let url = format!("http://127.0.0.1:{}/", models::OPENCODEX_PORT);
    platform::open_url(&url).map_err(|e| e.to_string())
}

/// Providers to offer in the composer.
///
/// The niche gateways are only useful once they have a key, and an entry that
/// cannot answer is worse than no entry: it looks like a choice. They appear
/// as soon as a key is saved in Settings, and the provider in use is always
/// listed so a saved session never points at something that is not there.
#[tauri::command]
pub async fn list_providers(state: State<'_, AppState>) -> Result<Vec<ProviderInfo>, String> {
    let settings = state.settings.lock().await;
    let current = models::Provider::parse(&settings.provider);
    Ok(models::provider_list()
        .into_iter()
        .filter(|p| {
            let provider = models::Provider::parse(p.id);
            if !matches!(
                provider,
                models::Provider::CheaperInference | models::Provider::Abliteration
            ) {
                return true;
            }
            provider == current || settings.key_for(provider).is_some()
        })
        .collect())
}

// ---- OpenCodex provider wrapper: set up any upstream provider from the UI ----

/// Run the `ocx` CLI with args, optionally piping `stdin_data` (for keys). In
/// Docker mode it runs the container's bundled ocx (`docker exec … ocx …`); in
/// host mode the host `ocx`.
/// Container-local scratch codex home for the bundled OpenCodex proxy.
const OCX_CODEX_HOME_CONTAINER: &str = "/tmp/hacksor-ocx-codex";

/// A PRIVATE codex home for the OpenCodex proxy Hacksor manages.
///
/// ocx injects its own root routing (`openai_base_url = <the proxy>`) into
/// whatever codex home it resolves, and its resolver falls back to `~/.codex`
/// when `CODEX_HOME` is unset. Unset, Hacksor's proxy therefore rewrites the
/// USER's personal codex config and silently redirects their standalone `codex`
/// CLI — subscription login and all — through Hacksor's proxy. In Docker that
/// reaches the real host file, because the user's home is bind-mounted.
///
/// Hacksor never needs that injection: its harness reaches the proxy through
/// the `[model_providers.*] base_url` it writes in its OWN codex home. So give
/// ocx a scratch home it may rewrite freely. Under Docker it is container-local
/// (`/tmp`), so it cannot touch the host at all; on the host it is a per-user
/// Hacksor directory. Either way it is never `~/.codex`, and never the codex
/// home the harness uses.
fn ocx_codex_home(docker: bool) -> String {
    if docker {
        return OCX_CODEX_HOME_CONTAINER.to_string();
    }
    let p = dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("hacksor")
        .join("ocx-codex");
    let _ = std::fs::create_dir_all(&p);
    p.to_string_lossy().into_owned()
}

async fn run_ocx(args: &[&str], stdin_data: Option<&str>, docker: bool) -> Result<String, String> {
    if docker {
        if !runtime_container_running().await {
            return Err("Runtime container is not running.".into());
        }
        // Every ocx invocation carries the private codex home, not just `start`:
        // several subcommands re-run the native-integration sync.
        // `sh -c '<script>' sh <argv...>` puts the ocx argv in "$@", so no arg
        // has to be quoted into the script. mkdir first: ocx aborts if
        // CODEX_HOME does not already exist.
        let home = ocx_codex_home(true);
        let mut cmd: Vec<String> = vec![
            "sh".into(),
            "-c".into(),
            format!("mkdir -p {home} && exec env CODEX_HOME={home} \"$@\""),
            "sh".into(),
            "ocx".into(),
        ];
        cmd.extend(args.iter().map(|s| s.to_string()));
        let (out, code) = runtime::exec_output(RUNTIME_CONTAINER, cmd, exec_user(), stdin_data).await?;
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
        cmd.env("CODEX_HOME", ocx_codex_home(false))
            .args(args)
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

/// What Hacksor found when an OpenCodex request came back "not logged in".
#[derive(Serialize, Default)]
pub struct OcxAuthState {
    /// The ocx upstream the error named (`anthropic`, `openai`, …).
    pub provider: String,
    /// True when ocx still holds a stored account for that upstream, i.e. the
    /// login the user already did is present and the failure was not a missing
    /// login at all.
    pub logged_in: bool,
    /// The account row ocx printed (identifier already masked by ocx).
    pub detail: String,
    /// True when the proxy was restarted to drop the credential it was holding.
    pub restarted: bool,
    /// ocx serving the proxy, and the host CLI, when both exist. Two different
    /// versions sharing `~/.opencodex` is its own source of auth flapping, so
    /// it is worth naming rather than leaving the user to guess.
    pub proxy_version: String,
    pub host_version: String,
}

/// An ocx upstream name, as it appears in `ocx login <provider>`. Anything else
/// is rejected rather than passed to a shell.
fn safe_provider_name(raw: &str) -> Option<String> {
    let name = raw.trim().to_lowercase();
    if name.is_empty()
        || name.len() > 40
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some(name)
}

/// The account row `ocx account current <provider>` printed for that provider,
/// and whether it is usable. Matched on the first column so `anthropic` never
/// picks up an `anthropic2` row, and the header is skipped by the same rule.
///
/// A paused account is deliberately out of rotation: restarting the proxy would
/// not bring it back, so it counts as not usable and is reported as it is.
fn parse_account_row(listing: &str, name: &str) -> Option<(String, bool)> {
    listing
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .find(|l| l.split_whitespace().next() == Some(name))
        .map(|row| (row.to_string(), !row.contains("paused")))
}

/// Recover from OpenCodex's "run `ocx login <provider>`" error.
///
/// ocx raises it when the proxy had no usable token for the upstream as it
/// built the request. That is not the same as the user never having logged in:
/// the stored credential sits in `~/.opencodex/auth.json`, which this machine's
/// container proxy and any host `ocx` share, and both can refresh it.
///
/// What was measured in the usage log: two bursts of six 401s each (codex
/// retrying one turn), and auth.json rewritten about a minute after the second
/// burst. So the proxy was sending a bearer the upstream had stopped accepting
/// while the login itself was still on disk — the exact case where telling the
/// user to log in again is the wrong advice. It is wrong twice over in Docker
/// mode, where `ocx` is in the container and not on their PATH at all.
///
/// So: ask ocx whether an account is still stored, and if one is, restart the
/// proxy so the next request resolves the credential afresh.
#[tauri::command]
pub async fn ocx_auth_check(state: State<'_, AppState>, provider: String) -> Result<OcxAuthState, String> {
    let Some(name) = safe_provider_name(&provider) else {
        return Err(format!("Not a provider name: {provider}"));
    };
    let docker = docker_runtime(&state).await;
    let mut out = OcxAuthState { provider: name.clone(), ..Default::default() };

    // `account current` is read-only and prints one row per stored credential.
    // A non-zero exit (no such provider configured) is a "no account" answer,
    // not a failure to report.
    let listing = run_ocx(&["account", "current", &name], None, docker).await.unwrap_or_default();
    if let Some((row, active)) = parse_account_row(&listing, &name) {
        out.detail = row;
        out.logged_in = active;
    }

    out.proxy_version = run_ocx(&["--version"], None, docker)
        .await
        .unwrap_or_default()
        .trim()
        .to_string();
    if docker {
        if let Some(bin) = which_bin("ocx") {
            if let Ok(o) = tokio::process::Command::new(bin).arg("--version").output().await {
                out.host_version = String::from_utf8_lossy(&o.stdout).trim().to_string();
            }
        }
    }

    if out.logged_in {
        restart_opencodex(&state).await;
        out.restarted = opencodex_up().await;
    }
    Ok(out)
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

/// One cheap, non-streaming judge call. Returns the assistant's raw content, or
/// `None` on any failure (transport error, bad status, unparseable) so every
/// caller can degrade to "no opinion" instead of surfacing an error.
///
/// The key is OPTIONAL: every provider is fronted by the local OpenCodex proxy,
/// and the OpenCodex selection deliberately has no Hacksor-side key (its
/// upstream keys live in ocx's own config). Requiring one here would silently
/// disable every judge for that provider. If the proxy does want a bearer, the
/// unauthenticated call just fails and we fall back to "no opinion" anyway.
async fn judge_call(
    state: &State<'_, AppState>,
    provider: &str,
    model: &str,
    sys: &str,
    user: &str,
) -> Option<String> {
    let p = Provider::parse(provider);
    let key = {
        let s = state.settings.lock().await;
        s.key_for(p).map(|k| k.to_string())
    };
    let key = key.filter(|k| !k.is_empty());
    let body = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": sys},
            {"role": "user", "content": user}
        ],
        "temperature": 0,
        "max_tokens": 200,
    });
    let client = reqwest::Client::builder().user_agent("hacksor/0.1").build().ok()?;
    let url = format!("{}/chat/completions", p.base_url());
    let mut req = client.post(url).json(&body);
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let text = resp.text().await.ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("choices")?
        .get(0)?
        .get("message")?
        .get("content")?
        .as_str()
        .map(String::from)
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

pub(crate) fn truncate(s: &str, n: usize) -> String {
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

#[derive(Deserialize)]
pub struct PhaseArgs {
    pub provider: String,
    /// The cheap judge model (NOT one of the two pipeline models).
    pub model: String,
    /// Phase the engagement is in right now: "recon" | "exploit".
    pub phase: String,
    /// What the user asked for on the turn that just finished.
    pub task: String,
    /// What the pipeline model just reported back.
    pub answer: String,
}

#[derive(Serialize)]
pub struct PhaseVerdict {
    /// "recon" | "exploit" | "unknown" (unknown ⇒ caller keeps the current phase).
    pub phase: String,
    pub reason: String,
}

/// Decide which half of the Auto pipeline should run the NEXT turn, by judging
/// what the engagement has actually established — not how the user worded their
/// request. Recon (MiMo) investigates and confirms; exploitation (DeepSeek) only
/// takes over once there is a specific, understood finding to act on, so it
/// never has to rediscover what recon already found.
///
/// Fail-safe: any error yields "unknown" and the caller keeps the current phase.
#[tauri::command]
pub async fn judge_phase(state: State<'_, AppState>, args: PhaseArgs) -> Result<PhaseVerdict, String> {
    let unknown = || PhaseVerdict { phase: "unknown".into(), reason: String::new() };
    let sys = "You route an authorized penetration test between two models in a pipeline.\n\
RECON: investigates, enumerates, and CONFIRMS whether a vulnerability is real. All discovery lives here.\n\
EXPLOIT: weaponizes and executes against an ALREADY-CONFIRMED, ALREADY-UNDERSTOOD finding. It must never have to discover anything.\n\
\n\
You are given the phase that just ran, the user's request, and what the model reported. Decide which phase should handle the NEXT turn.\n\
Choose \"exploit\" ONLY when ALL of these hold: a specific vulnerability is confirmed (not merely suspected or scanner-reported); the exact target of the attack is known (endpoint/parameter/injection point or equivalent); and enough context exists to act without further investigation.\n\
Choose \"recon\" when anything is still unknown, unconfirmed, or newly discovered: no finding yet, a lead that needs validation, a fresh endpoint/host/surface, exploitation that failed for lack of information, or the user asking to look deeper.\n\
The user asking to exploit something does NOT by itself justify \"exploit\" — the finding must actually be established first. Staying in recon is the safe default.\n\
Reply with ONLY compact JSON, no prose: {\"phase\":\"recon\"|\"exploit\",\"reason\":\"<=120 chars, cite the concrete finding or what is still missing\"}";
    let user = format!(
        "PHASE THAT JUST RAN: {}\n\nUSER REQUEST:\n{}\n\nWHAT THE MODEL REPORTED:\n{}",
        if args.phase == "exploit" { "exploit" } else { "recon" },
        truncate(&args.task, 3000),
        truncate(&args.answer, 8000)
    );
    match judge_call(&state, &args.provider, &args.model, sys, &user).await {
        Some(content) => Ok(parse_phase_verdict(&content)),
        None => Ok(unknown()),
    }
}

#[derive(Deserialize)]
pub struct TitleArgs {
    pub provider: String,
    pub model: String,
    pub text: String,
}

/// Name a chat from its first message.
///
/// Keeping the raw prompt as the title makes every sidebar row a wall of the
/// same opening sentence. This asks for a few words instead. It is advisory:
/// an empty answer means the caller keeps the truncated prompt, so a model
/// that is slow, down, or chatty never blocks naming a chat.
#[tauri::command]
pub async fn suggest_title(state: State<'_, AppState>, args: TitleArgs) -> Result<String, String> {
    let sys = "You name chat threads. Reply with a title of 2 to 5 words for the user's request, \
and NOTHING else: no quotes, no trailing period, no prose, no explanation, no markdown. \
Name the subject or task, not the fact that it is a request (\"Nmap Scan Of Staging\", not \
\"User Asks For A Scan\"). Keep the target's own words where they are short and concrete.";
    let user = format!("Name this request:\n\n{}", truncate(args.text.trim(), 2000));
    let raw = judge_call(&state, &args.provider, &args.model, sys, &user).await;
    Ok(raw.map(|r| clean_title(&r)).unwrap_or_default())
}

/// Reduce whatever the model replied with to a usable title, or "" to mean
/// "not usable, keep the fallback".
///
/// Models ignore "a few words" in every direction: quoting the title, prefixing
/// "Title:", answering in several lines, or writing a sentence. Take the first
/// non-empty line, strip the decoration, and give up rather than letting a
/// paragraph into the sidebar.
fn clean_title(raw: &str) -> String {
    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let line = line
        .trim_start_matches("Title:")
        .trim_start_matches("title:")
        .trim();
    let line = line.trim_matches(|c| c == '"' || c == '\'' || c == '`' || c == '*' || c == '#');
    let line = line.trim().trim_end_matches(['.', ':', '!']).trim();
    // Collapse runs of whitespace so a stray newline or tab cannot widen a row.
    let words: Vec<&str> = line.split_whitespace().collect();
    if words.is_empty() {
        return String::new();
    }
    // A sentence is not a title: anything this long means the model ignored the
    // instruction, and the truncated prompt is a better fallback than prose.
    if words.len() > 8 {
        return String::new();
    }
    let out = words.join(" ");
    if out.chars().count() > 60 {
        return String::new();
    }
    out
}

/// Parse the judge's reply. Anything unexpected — no JSON, a phase outside the
/// two known ones, junk — becomes "unknown", which keeps the current phase.
fn parse_phase_verdict(content: &str) -> PhaseVerdict {
    let unknown = PhaseVerdict { phase: "unknown".into(), reason: String::new() };
    let (Some(a), Some(b)) = (content.find('{'), content.rfind('}')) else {
        return unknown;
    };
    if b <= a {
        return unknown;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&content[a..=b]) else {
        return unknown;
    };
    match v.get("phase").and_then(|x| x.as_str()) {
        Some(p @ ("recon" | "exploit")) => PhaseVerdict {
            phase: p.to_string(),
            reason: v.get("reason").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        },
        _ => unknown,
    }
}

/// ocx's readiness status (`ocx ready --json` → `ready|pending|failed|…`). The
/// proxy answers HTTP on `/v1/models` about a second after launch, but it
/// discovers each upstream's catalog asynchronously AFTER that, so "the port is
/// open" does not mean "the catalog is populated".
async fn ocx_status(docker: bool) -> String {
    match run_ocx(&["ready", "--json"], None, docker).await {
        // A non-ready status exits 1, which run_ocx reports as Err — the JSON we
        // want is in the output either way.
        // Lenient for the same reason as the catalog: ocx advisories ride along
        // on stderr, and a strict parse here reported "" — which the caller
        // reads as "keep waiting", so it polled out instead of seeing `ready`.
        Ok(o) | Err(o) => models::parse_json_lenient::<serde_json::Value>(&o)
            .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(String::from))
            .unwrap_or_default(),
    }
}

/// Read a provider's slice of ocx's live catalog, waiting for upstream discovery
/// to finish.
///
/// Two behaviours of ocx make a single read unreliable:
///   * discovery runs in the background after the proxy starts answering, and a
///     slow gateway takes seconds (Cheaper Inference's ~480 KB catalog measured
///     3–13s), so the first read is normally empty — ocx reports `pending`;
///   * when a discovery fetch throws (it times out on a slow upstream), ocx
///     reports `failed`, falls back to configured models only, and NEVER retries
///     — the catalog stays empty until the proxy restarts.
///
/// So: poll while ocx says `pending`, and on a terminal `failed` restart the
/// proxy once to retry discovery. Falling through with an empty list lets the
/// caller's diagnostic explain a genuinely bad key.
///
/// `ready` is NOT a terminal answer either: it is the proxy's own readiness and
/// flips as soon as ocx answers, while each upstream's catalog discovery is
/// still in flight. Switching provider right after startup asks for an upstream
/// that has not landed yet — measured on the bundled ocx, `vercel-ai-gateway`
/// went from 0 to 341 models with the status sitting at `ready` throughout. So
/// a ready-but-empty read is retried a bounded number of rounds before it is
/// believed.
/// The model count out of an `ocx provider test` line, e.g. "connected — 391
/// models available." → `Some(391)`. `None` when the line carries no count.
fn models_available_in(diag: &str) -> Option<u32> {
    let idx = diag.find("models available")?;
    diag[..idx]
        .split_whitespace()
        .next_back()
        .and_then(|w| w.parse().ok())
}

async fn read_live_catalog(
    state: &State<'_, AppState>,
    p: Provider,
    docker: bool,
) -> Result<Vec<ModelInfo>, String> {
    // Rounds to keep polling a `ready` proxy that has nothing for this upstream
    // (~1.2s each) before accepting the empty catalog as the real answer.
    const READY_EMPTY_ROUNDS: u32 = 8;
    let mut restarted = false;
    let mut ready_empty = 0u32;
    for attempt in 0..16 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        }
        let json = run_ocx(&["models", "live", "--json"], None, docker)
            .await
            .map_err(|e| format!("Could not read the model catalog from OpenCodex: {e}"))?;
        let models = models::parse_live_models(&json, p.ocx_upstream());
        if !models.is_empty() {
            return Ok(models);
        }
        // Empty: decide whether waiting can still help.
        match ocx_status(docker).await.as_str() {
            // The port answers (ensure_opencodex checked) yet ocx reports no
            // /readyz identity — this proxy predates the bundled ocx, so it is a
            // STALE process still serving the provider config it loaded at its
            // own startup and it will never learn about a newly added upstream.
            // Only a restart helps. Give it a few rounds first: a proxy that is
            // merely still booting can report this briefly.
            "unreachable" if !restarted && attempt >= 3 => {
                restarted = true;
                restart_opencodex(state).await;
                ensure_opencodex(state).await?;
            }
            // Keep waiting. "failed" is deliberately NOT a restart trigger: ocx
            // also reports it when its Codex *sync* fails, which says nothing
            // about model discovery — restarting on it kills the in-flight
            // discovery and empties the catalog for every provider.
            "pending" | "failed" | "unreachable" | "" => {}
            // Ready, but this upstream's discovery may simply not have landed
            // yet (see the note above) — give it a few more rounds, then stop.
            _ => {
                ready_empty += 1;
                if ready_empty >= READY_EMPTY_ROUNDS {
                    break;
                }
            }
        }
    }
    Ok(Vec::new())
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
    let docker = docker_runtime(&state).await;
    // ocx must be up (with its upstreams configured from the saved keys) to
    // enumerate the live catalog.
    ensure_opencodex(&state).await?;
    // ocx exposes provider catalogs ONLY through `models live` — /v1/models
    // carries just its own native models. Read the live catalog (JSON) and keep
    // this provider's upstream.
    let models = read_live_catalog(&state, p, docker).await?;
    // A keyed upstream (OpenRouter/Vercel) with a key but zero models: explain why
    // instead of showing a silent empty list.
    if models.is_empty() {
        if let Some(upstream) = p.ocx_upstream() {
            let diag = match run_ocx(&["provider", "test", upstream], None, docker).await {
                Ok(o) | Err(o) => o.to_lowercase(),
            };
            let reason = if diag.contains("401") || diag.contains("unauthorized") {
                "the API key was rejected (HTTP 401). Double-check the key in Settings → Providers."
            } else if diag.contains("403") {
                "the key was forbidden (HTTP 403) — it may lack model access."
            } else if diag.contains("not running") {
                "the proxy isn't ready yet. Try again in a moment."
            } else if diag.contains("connected") || diag.contains(": ok") || diag.contains("success") {
                // `provider test` does its own live call and prints the real
                // count ("Connected — 391 models available."). A non-zero count
                // here means the upstream is fine and it is the proxy's catalog
                // that is behind, so don't send the user to their dashboard.
                match models_available_in(&diag) {
                    Some(n) if n > 0 => {
                        return Err(format!(
                            "No {} models yet — the upstream reports {n} models, but the proxy's catalog hasn't picked them up. Reopen this menu in a few seconds.",
                            p.display()
                        ))
                    }
                    _ => "the key works, but no models are exposed for it — make sure models are enabled in your provider dashboard.",
                }
            } else {
                "the key may be invalid, or the proxy is still starting — reopen this menu in a moment."
            };
            return Err(format!("No {} models — {reason}", p.display()));
        }
    }
    Ok(models)
}

#[derive(Serialize)]
pub struct SettingsView {
    pub provider: String,
    pub has_openrouter_key: bool,
    pub has_vercel_key: bool,
    pub has_cheaper_inference_key: bool,
    pub has_abliteration_key: bool,
    pub personality: Option<String>,
    pub custom_instructions: Option<String>,
    pub working_dir: String,
    pub kali_mode: bool,
    pub runtime: String,
    pub services_mode: String,
    pub warn_without_vpn: bool,
    pub limit_runtime_mount: bool,
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
        has_cheaper_inference_key: s.key_for(Provider::CheaperInference).is_some(),
        has_abliteration_key: s.key_for(Provider::Abliteration).is_some(),
        personality: s.personality.clone(),
        custom_instructions: s.custom_instructions.clone(),
        working_dir: s.working_dir.clone(),
        kali_mode: s.kali_mode,
        runtime: s.runtime.clone(),
        services_mode: s.services_mode.clone(),
        warn_without_vpn: s.warn_without_vpn,
        limit_runtime_mount: s.limit_runtime_mount,
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
    pub cheaper_inference_api_key: Option<String>,
    pub abliteration_api_key: Option<String>,
    pub personality: Option<String>,
    pub custom_instructions: Option<String>,
    pub working_dir: Option<String>,
    pub kali_mode: Option<bool>,
    pub runtime: Option<String>,
    pub services_mode: Option<String>,
    pub warn_without_vpn: Option<bool>,
    pub limit_runtime_mount: Option<bool>,
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
    if let Some(k) = args.cheaper_inference_api_key {
        let t = k.trim().to_string();
        let v = if t.is_empty() { None } else { Some(t) };
        let changed = v != s.cheaper_inference_api_key;
        needs_restart |= changed;
        keys_changed |= changed;
        s.cheaper_inference_api_key = v;
    }
    if let Some(k) = args.abliteration_api_key {
        let t = k.trim().to_string();
        let v = if t.is_empty() { None } else { Some(t) };
        let changed = v != s.abliteration_api_key;
        needs_restart |= changed;
        keys_changed |= changed;
        s.abliteration_api_key = v;
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
    if let Some(v) = args.warn_without_vpn {
        s.warn_without_vpn = v;
    }
    if let Some(v) = args.limit_runtime_mount {
        // Takes effect when the container is next created, not mid-session.
        s.limit_runtime_mount = v;
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

    // A changed provider key configures the OpenCodex upstream it routes through,
    // then restarts ocx so it picks up the change
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
pub async fn read_transcript(
    state: State<'_, AppState>,
    thread_id: String,
    limit: Option<usize>,
) -> Result<Transcript, String> {
    // Off the main thread: a sync #[tauri::command] runs on it, and a large
    // rollout (measured here: 22 MB / 2734 records) froze the whole UI while it
    // was read and parsed.
    let home = state.codex_home.clone();
    tokio::task::spawn_blocking(move || {
        let Some(path) = find_rollout(&home, &thread_id) else {
            return Ok(Transcript::default());
        };
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let tokens = last_context_tokens(&text);
        let mut tr = slice_transcript(parse_rollout(&text), limit);
        tr.tokens = tokens;
        Ok(tr)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// How many past prompts the composer's history keeps. Bounds the payload for
/// a chat whose every turn pastes a large blob, without truncating any single
/// prompt (a recalled prompt must be exactly what was sent).
const PROMPT_HISTORY_MAX: usize = 500;

/// A transcript slice: the tail the UI renders, how many records exist in all,
/// and the full prompt history.
#[derive(Serialize, Default)]
pub struct Transcript {
    pub items: Vec<serde_json::Value>,
    pub total: usize,
    pub prompts: Vec<String>,
    /// Characters across the WHOLE conversation, counted before the tail is
    /// sliced off. Only used when `tokens` is absent.
    pub chars: usize,
    /// Real context occupancy from the rollout's last `token_usage_record`,
    /// when there is one. Exact, and already reflects any compaction, so it is
    /// preferred over the character estimate.
    pub tokens: Option<u64>,
}

/// Keep only the tail the UI renders, while reporting the true total and the
/// FULL prompt history.
///
/// Prompts are collected before trimming, so up-arrow recall still reaches
/// prompts that scrolled off the top, and they are never truncated (a recalled
/// prompt must be exactly what was sent). That costs almost nothing: user text
/// measured 3-74 KB per chat against a 12 MB transcript.
/// Roughly how much of the context window one transcript record occupies.
///
/// Walks the fields that actually carry conversation — message text, reasoning,
/// a command and its output — rather than the whole record, so ids, timestamps
/// and other metadata are not counted as tokens.
/// Context occupancy at the last turn, from the rollout.
///
/// codex writes a `token_usage_record` per response but never sends the
/// matching notification over the app-server on every provider path, so the
/// live meter had nothing to show. The file has it: the LAST record's
/// `usage.total_tokens` is what the model was handed for that turn, which is
/// the context size — and because it is the last one it already accounts for
/// any compaction along the way.
///
/// Note `turn_token_usage` is cumulative across the whole session (millions of
/// tokens on a long engagement) and is NOT the context size.
fn last_context_tokens(text: &str) -> Option<u64> {
    let mut last = None;
    for line in text.lines() {
        if !line.contains("\"token_usage_record\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let usage = &v["payload"]["usage"];
        if let Some(total) = usage["total_tokens"].as_u64() {
            last = Some(total);
        } else if let Some(input) = usage["input_tokens"].as_u64() {
            last = Some(input + usage["output_tokens"].as_u64().unwrap_or(0));
        }
    }
    last
}

fn transcript_item_chars(item: &serde_json::Value) -> usize {
    let mut n = 0usize;
    for key in ["text", "command", "output", "aggregated_output", "summary"] {
        if let Some(v) = item.get(key).and_then(|v| v.as_str()) {
            n += v.len();
        }
    }
    if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
        for p in parts {
            if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                n += t.len();
            } else if let Some(t) = p.as_str() {
                n += t.len();
            }
        }
    }
    n
}

fn slice_transcript(mut items: Vec<serde_json::Value>, limit: Option<usize>) -> Transcript {
    let total = items.len();
    let chars: usize = items.iter().map(transcript_item_chars).sum();
    let mut prompts: Vec<String> = items
        .iter()
        .filter(|i| i["type"] == "userMessage")
        .filter_map(|i| i["content"][0]["text"].as_str().map(str::to_string))
        .collect();
    if prompts.len() > PROMPT_HISTORY_MAX {
        prompts.drain(..prompts.len() - PROMPT_HISTORY_MAX);
    }
    // Ship only what the UI will render; the rest is fetched on demand when the
    // reader asks for earlier messages. A full transcript was 12.46 MB over the
    // IPC bridge to render 1.29 MB of it — ~90% discarded on every chat open.
    if let Some(n) = limit.filter(|n| *n > 0 && *n < total) {
        items.drain(..total - n);
    }
    Transcript { items, total, prompts, chars, tokens: None }
}

/// The provider a thread was CREATED under, read from the rollout's
/// `session_meta.model_provider`. codex binds a thread to its creation provider
/// (turn/start & thread/resume ignore `modelProvider`), so the frontend uses
/// this to know when a provider switch must start a fresh thread. Returns the
/// codex provider id (matches Hacksor's ids: openrouter/vercel/opencodex), or
/// an empty string when unknown.
#[tauri::command]
pub async fn thread_provider(state: State<'_, AppState>, thread_id: String) -> Result<String, String> {
    let home = state.codex_home.clone();
    tokio::task::spawn_blocking(move || {
        use std::io::BufRead;
        let Some(path) = find_rollout(&home, &thread_id) else { return Ok(String::new()) };
        // session_meta is the FIRST record and provider_from_rollout only ever
        // inspects the first 5 lines, so read just the head. This used to
        // read_to_string the whole rollout — tens of MB — to look at line 1.
        let f = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        let mut head = String::new();
        for line in std::io::BufReader::new(f).lines().take(5).map_while(Result::ok) {
            head.push_str(&line);
            head.push('\n');
        }
        Ok(provider_from_rollout(&head).unwrap_or_default())
    })
    .await
    .map_err(|e| e.to_string())?
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

/// Render the kept turns as one bounded block, newest-first-priority: returns
/// the body and whether anything older had to be dropped to fit `cap`. Shared by
/// the provider-switch carryover and a branch's inline digest, which wrap it in
/// their own framing.
fn carryover_body(items: &[serde_json::Value], cap: usize) -> (String, bool) {
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
        return (String::new(), false);
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
    (body.join("\n"), truncated)
}

/// Pure formatter for `build_carryover` (unit-tested).
fn carryover_from_items(items: &[serde_json::Value], cap: usize) -> String {
    let (body, truncated) = carryover_body(items, cap);
    if body.is_empty() {
        return String::new();
    }
    let head = if truncated {
        "This is the RECENT part of an ongoing conversation (earlier turns omitted), migrated because the provider/model changed. Continue it seamlessly using this context — do not re-introduce yourself, re-run completed steps, or repeat findings already established below."
    } else {
        "This is the conversation so far, migrated because the provider/model changed. Continue it seamlessly using this context — do not re-introduce yourself, re-run completed steps, or repeat findings already established below."
    };
    format!("<conversation_carryover>\n{head}\n\n{body}\n</conversation_carryover>")
}

// ---- branch (fork a chat at one message) ----

/// What a branch needs to carry: the extracted context file, a bounded inline
/// digest for the branch's first message, and what was in the cut.
#[derive(Serialize, Default)]
pub struct BranchContext {
    /// Path the AGENT reads (container-side in Docker mode).
    pub path: String,
    /// Path on the host, for the UI to show and for diagnostics.
    pub host_path: String,
    /// Bounded digest of the kept conversation, prepended to the first message
    /// so the branch has usable context even before it opens the file.
    pub inline: String,
    /// How many transcript items the branch keeps.
    pub items: usize,
    /// How many upstream compactions the kept range spans. The rollout is
    /// append-only, so the pre-compaction turns are still on disk and are
    /// written to the file even though codex's own resume would only replay the
    /// post-compaction summary.
    pub compactions: usize,
}

/// Keep the conversation up to the `ordinal`-th user message (1-based).
///
/// `include_reply` extends the cut past that prompt's answer (everything up to
/// just before the NEXT user message), which is what branching from an answer
/// means. Everything below the cut is dropped: that is the whole point of a
/// branch, so the new chat carries the branch point and what came before it and
/// nothing after.
fn truncate_at_user(items: &[serde_json::Value], ordinal: usize, include_reply: bool) -> Vec<serde_json::Value> {
    if ordinal == 0 {
        return Vec::new();
    }
    // Index of every user message, so both the cut point and the next prompt
    // (the end of the answer) are a lookup rather than a second scan.
    let users: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, it)| it["type"] == "userMessage")
        .map(|(i, _)| i)
        .collect();
    let Some(&at) = users.get(ordinal - 1) else {
        // Past the end (a live message not yet on disk): keep everything.
        return items.to_vec();
    };
    let end = if include_reply {
        users.get(ordinal).copied().unwrap_or(items.len())
    } else {
        at + 1
    };
    items[..end].to_vec()
}

/// Render a kept range as the markdown the branch reads. Commands keep their
/// output (truncated): in a pentest the discovered ports, endpoints and creds
/// ARE the context. Reasoning is dropped — it is verbose, provider-specific,
/// and not something the next model should imitate.
fn branch_markdown(items: &[serde_json::Value], compactions: usize) -> String {
    let mut out = String::new();
    out.push_str("# Branch context\n\n");
    out.push_str(
        "This is the full conversation up to the point this chat was branched from. \
         It is the only record of it: the branch starts a fresh thread, so nothing here \
         is in the model's context until it is read.\n\n",
    );
    if compactions > 0 {
        out.push_str(&format!(
            "The source conversation was compacted {compactions} time(s) before the branch point. \
             Everything from before each compaction is included below, recovered from the \
             on-disk transcript, so no context is missing here even though the original chat \
             had already dropped it.\n\n",
        ));
    }
    out.push_str("---\n\n");
    for it in items {
        match it["type"].as_str() {
            Some("userMessage") => {
                let t = it["content"][0]["text"].as_str().unwrap_or("").trim();
                if !t.is_empty() {
                    out.push_str(&format!("## User\n\n{t}\n\n"));
                }
            }
            Some("agentMessage") => {
                let t = it["text"].as_str().unwrap_or("").trim();
                if !t.is_empty() {
                    out.push_str(&format!("## Assistant\n\n{t}\n\n"));
                }
            }
            Some("commandExecution") => {
                let cmd = it["command"].as_str().unwrap_or("").trim();
                if cmd.is_empty() {
                    continue;
                }
                let code = it["exitCode"].as_i64();
                let out_short = truncate(it["aggregatedOutput"].as_str().unwrap_or("").trim(), 4_000);
                out.push_str("### Command\n\n```\n$ ");
                out.push_str(cmd);
                if let Some(c) = code {
                    out.push_str(&format!("\n(exit {c})"));
                }
                if !out_short.is_empty() {
                    out.push('\n');
                    out.push_str(&out_short);
                }
                out.push_str("\n```\n\n");
            }
            _ => {}
        }
    }
    out
}

/// The digest prepended to a branch's first message. Framed as a branch, not a
/// provider migration: the branch is a fresh thread, so without this the model
/// starts with no idea that any of it happened.
fn branch_inline(items: &[serde_json::Value], cap: usize) -> String {
    let (body, truncated) = carryover_body(items, cap);
    if body.is_empty() {
        return String::new();
    }
    let head = if truncated {
        "This chat is a BRANCH of an earlier conversation, taken at the last turn below. This digest holds its recent part; the full conversation up to the branch point is in the file named after it. Continue from here — do not re-introduce yourself or redo work already done below."
    } else {
        "This chat is a BRANCH of an earlier conversation, taken at the last turn below. Continue from here — do not re-introduce yourself or redo work already done below."
    };
    format!("<branch_context>\n{head}\n\n{body}\n</branch_context>")
}

/// Extract a branch's context: everything up to the branch point, written to a
/// file the agent can read.
///
/// A branch deliberately does NOT use codex's `thread/fork`. Fork copies the
/// whole thread and offers no way to cut it at a message (verified against
/// app-server 0.162.1: `thread/fork` ignores unknown params, and `thread/revert`
/// with `beforeTurnId` leaves the turn list intact), so a forked chat carried
/// the messages BELOW the branch point as context. The branch gets a fresh
/// thread instead — which also means it is a normal chat with the current
/// permission, not a replay of a thread whose stored sandbox may be restrictive.
#[tauri::command]
pub async fn branch_context(
    state: State<'_, AppState>,
    thread_id: String,
    user_ordinal: usize,
    include_reply: bool,
) -> Result<BranchContext, String> {
    let home = state.codex_home.clone();
    // The path the agent is told to read has to be the path IT sees: the
    // container's in Docker mode, the host's when the agent runs on the host.
    let docker_mode = { state.settings.lock().await.runtime == "docker" };
    tokio::task::spawn_blocking(move || {
        let Some(path) = find_rollout(&home, &thread_id) else {
            return Ok(BranchContext::default());
        };
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let items = truncate_at_user(&parse_rollout(&text), user_ordinal, include_reply);
        let compactions = compactions_before(&text, user_ordinal, include_reply);
        let dir = home.join("notes").join("branches");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let file = dir.join(format!("branch-{stamp}.md"));
        std::fs::write(&file, branch_markdown(&items, compactions)).map_err(|e| e.to_string())?;
        Ok(BranchContext {
            path: if docker_mode {
                crate::platform::to_container_path(&file)
            } else {
                file.to_string_lossy().into_owned()
            },
            host_path: file.to_string_lossy().into_owned(),
            inline: branch_inline(&items, 8_000),
            items: items.len(),
            compactions,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---- version, update check, and release notes ----

/// This build's own changelog, so "what's new" works offline and always matches
/// the binary. The release workflow tags the commit semantic-release made (the
/// bundle jobs check out `v<version>`), so a shipped build always contains its
/// own section.
const CHANGELOG: &str = include_str!("../../CHANGELOG.md");

/// Where releases are published. Used for the update check and the download link.
const RELEASES_API: &str = "https://api.github.com/repos/louislafosse/hacksor/releases/latest";
const RELEASES_PAGE: &str = "https://github.com/louislafosse/hacksor/releases/latest";

/// The running version, straight from the crate (Cargo.toml and
/// tauri.conf.json are both bumped by semantic-release).
#[tauri::command]
pub fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[derive(Serialize, Default)]
pub struct UpdateStatus {
    pub current: String,
    /// The newest published version, or empty when the check could not run.
    pub latest: String,
    pub up_to_date: bool,
    /// Release page for the download.
    pub url: String,
    /// The newer release's notes, for the "update available" panel.
    pub notes: String,
    /// Why the check failed, shown instead of a misleading "up to date".
    pub error: String,
}

/// Compare two semver-ish versions. Only the numeric `major.minor.patch` part is
/// compared; a pre-release suffix is ignored, which is deliberate — this only
/// ever answers "is the published release newer than what is running".
fn version_newer(latest: &str, current: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.trim()
            .trim_start_matches(['v', 'V'])
            .split(['-', '+'])
            .next()
            .unwrap_or("")
            .split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    }
    let (a, b) = (parts(latest), parts(current));
    if a.iter().all(|n| *n == 0) {
        return false; // unparseable: never claim an update
    }
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// Ask GitHub for the newest release and compare it with the running build.
///
/// Read-only and best effort: a failed check reports the error rather than
/// claiming the app is up to date, because "up to date" is exactly the wrong
/// thing to show when the check never reached the network.
#[tauri::command]
pub async fn check_update() -> Result<UpdateStatus, String> {
    let current = app_version();
    let mut out = UpdateStatus {
        current: current.clone(),
        url: RELEASES_PAGE.to_string(),
        ..Default::default()
    };
    let client = match reqwest::Client::builder()
        .user_agent(format!("hacksor/{current}"))
        .timeout(std::time::Duration::from_secs(12))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            out.error = e.to_string();
            return Ok(out);
        }
    };
    let res = match client.get(RELEASES_API).send().await {
        Ok(r) => r,
        Err(e) => {
            out.error = format!("could not reach GitHub: {e}");
            return Ok(out);
        }
    };
    if !res.status().is_success() {
        out.error = format!("GitHub returned {}", res.status());
        return Ok(out);
    }
    let body: serde_json::Value = match res.json().await {
        Ok(v) => v,
        Err(e) => {
            out.error = format!("unreadable release feed: {e}");
            return Ok(out);
        }
    };
    let tag = body["tag_name"].as_str().unwrap_or("").trim_start_matches('v').to_string();
    if tag.is_empty() {
        out.error = "the release feed has no tag_name".into();
        return Ok(out);
    }
    if let Some(u) = body["html_url"].as_str() {
        out.url = u.to_string();
    }
    out.up_to_date = !version_newer(&tag, &current);
    out.latest = tag;
    if !out.up_to_date {
        out.notes = body["body"].as_str().unwrap_or("").trim().to_string();
    }
    Ok(out)
}

/// Open a Hacksor release page in the browser.
///
/// Deliberately narrow: anything that is not a release URL of this repository
/// is ignored in favour of the releases page, so the only thing the frontend
/// can ever open through here is our own release listing.
#[tauri::command]
pub fn open_release_page(url: Option<String>) -> Result<(), String> {
    let target = url
        .filter(|u| is_release_url(u))
        .unwrap_or_else(|| RELEASES_PAGE.to_string());
    platform::open_url(&target).map_err(|e| e.to_string())
}

fn is_release_url(u: &str) -> bool {
    u.starts_with("https://github.com/louislafosse/hacksor/releases/")
}

/// One version's section of the changelog.
///
/// semantic-release writes `# [1.6.0](…) (date)` for a minor and
/// `## [1.5.3](…)` for a patch, so a section runs from its own heading to the
/// next heading of any level.
fn changelog_section(text: &str, version: &str) -> String {
    let is_heading = |l: &str| {
        let t = l.trim_start_matches('#').trim_start();
        l.starts_with('#') && (t.starts_with('[') || t.starts_with(|c: char| c.is_ascii_digit()))
    };
    let heading_version = |l: &str| -> String {
        l.trim_start_matches('#')
            .trim()
            .trim_start_matches('[')
            .split(|c: char| c == ']' || c == '(' || c.is_whitespace())
            .next()
            .unwrap_or("")
            .to_string()
    };
    let mut out: Vec<&str> = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if is_heading(line) {
            if inside {
                break;
            }
            if heading_version(line) == version {
                inside = true;
                out.push(line);
            }
            continue;
        }
        if inside {
            out.push(line);
        }
    }
    out.join("\n").trim().to_string()
}

#[derive(Serialize)]
pub struct ReleaseNotes {
    pub version: String,
    pub notes: String,
    /// The version the user last saw, for "1.5.3 -> 1.6.0" framing.
    pub previous: String,
}

/// The notes to show once, on the first run after an update.
///
/// Returns nothing on a fresh install (no previous version, so nothing
/// "changed") and nothing when this version has already been acknowledged.
/// Acknowledgement is a SEPARATE command so a crash between the two does not
/// swallow the notes.
#[tauri::command]
pub async fn pending_release_notes(state: State<'_, AppState>) -> Result<Option<ReleaseNotes>, String> {
    let current = app_version();
    let seen = { state.settings.lock().await.last_seen_version.clone() };
    let Some(previous) = seen.filter(|v| !v.trim().is_empty()) else {
        // Fresh install: record the version so the NEXT update shows notes.
        let mut s = state.settings.lock().await;
        s.last_seen_version = Some(current);
        let _ = s.save(&state.codex_home);
        return Ok(None);
    };
    if previous == current {
        return Ok(None);
    }
    let notes = changelog_section(CHANGELOG, &current);
    if notes.is_empty() {
        // No section for this build (a local dev build between releases):
        // nothing useful to show, so don't nag.
        let mut s = state.settings.lock().await;
        s.last_seen_version = Some(current);
        let _ = s.save(&state.codex_home);
        return Ok(None);
    }
    Ok(Some(ReleaseNotes { version: current, notes, previous }))
}

/// Mark this version's notes as seen (called once the user has them on screen).
#[tauri::command]
pub async fn ack_version(state: State<'_, AppState>) -> Result<(), String> {
    let mut s = state.settings.lock().await;
    s.last_seen_version = Some(app_version());
    s.save(&state.codex_home).map_err(|e| e.to_string())
}

/// This build's own release notes, for the "What's new" button in Settings.
#[tauri::command]
pub fn current_release_notes() -> ReleaseNotes {
    let current = app_version();
    ReleaseNotes {
        notes: changelog_section(CHANGELOG, &current),
        version: current,
        previous: String::new(),
    }
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
pub async fn list_recents(state: State<'_, AppState>) -> Result<Vec<RecentChat>, String> {
    let root = state.codex_home.join("sessions");
    // Walking the store and previewing every rollout is disk work — keep it off
    // the main thread so the sidebar populating never freezes the window.
    tokio::task::spawn_blocking(move || list_recents_blocking(root))
        .await
        .map_err(|e| e.to_string())?
}

fn list_recents_blocking(root: std::path::PathBuf) -> Result<Vec<RecentChat>, String> {
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

/// Wrappers codex injects as `user` messages (environment, permissions, skills,
/// world state). They are not prompts: they must not show in a transcript and
/// must not be counted when a prompt is named by its position.
const SKIP_USER_PREFIXES: [&str; 7] = [
    "<environment_context",
    "<model_switch",
    "<permissions",
    "<skills_instructions",
    "<user_instructions",
    "<world_state",
    "<worldstate",
];

/// Is this `user`-role content an actual prompt rather than an injected wrapper?
fn is_real_user_text(content: &str) -> bool {
    let t = content.trim_start();
    !t.is_empty() && !SKIP_USER_PREFIXES.iter().any(|p| t.starts_with(p))
}

/// How many compactions the kept range spans, i.e. those recorded BEFORE the
/// branch point. Counted on the raw lines because `parse_rollout` drops the
/// `compacted` records — which is exactly why the pre-compaction turns survive
/// into a branch at all. Counting them is what lets the branch say so.
fn compactions_before(text: &str, ordinal: usize, include_reply: bool) -> usize {
    // The cut ends at the ordinal-th prompt, or just before the next one when
    // the answer is kept too.
    let stop_at = if include_reply { ordinal + 1 } else { ordinal };
    let mut prompts = 0usize;
    let mut compactions = 0usize;
    for line in text.lines() {
        let Ok(o) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
        if o.get("type").and_then(|v| v.as_str()) == Some("compacted") {
            compactions += 1;
            continue;
        }
        let pay = o.get("payload").filter(|p| p.is_object()).unwrap_or(&o);
        if pay.get("type").and_then(|v| v.as_str()) != Some("message")
            || pay.get("role").and_then(|v| v.as_str()) != Some("user")
        {
            continue;
        }
        if is_real_user_text(&collect_text(pay.get("content"))) {
            prompts += 1;
            if prompts >= stop_at {
                break;
            }
        }
    }
    compactions
}

fn parse_rollout(text: &str) -> Vec<serde_json::Value> {
    use serde_json::{json, Value};
    use std::collections::{HashMap, VecDeque};

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
                    if !is_real_user_text(&content) {
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
/// Where the in-container ocx writes its startup output, so a failure to come up
/// can report ocx's own error instead of a bare timeout.
const OCX_START_LOG: &str = "/tmp/hacksor-ocx-start.log";
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
async fn pull_runtime_image(app: &AppHandle, codex_home: &std::path::Path) -> Result<(), String> {
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
    // Remember WHICH manifest this is, so a later start can tell whether the
    // registry has moved on. Comparing Docker's own digests is not reliable
    // here: a locally built image and a pulled one do not describe their
    // content the same way, so the pulled digest is recorded explicitly.
    if let Some(d) = runtime::remote_digest(image).await {
        record_runtime_digest(codex_home, &d);
    }
    let _ = app.emit("hacksor://runtime", serde_json::json!({
        "phase": "pulling", "percent": 100, "message": "Runtime downloaded."
    }));
    Ok(())
}

/// Where the digest of the runtime image we pulled is remembered.
fn runtime_digest_file(codex_home: &std::path::Path) -> std::path::PathBuf {
    codex_home.join("runtime-image.json")
}

fn record_runtime_digest(codex_home: &std::path::Path, digest: &str) {
    let body = serde_json::json!({
        "image": RUNTIME_REGISTRY_IMAGE,
        "digest": digest,
        "pulled_at": chrono::Utc::now().to_rfc3339(),
    });
    let _ = std::fs::write(runtime_digest_file(codex_home), body.to_string());
}

fn recorded_runtime_digest(codex_home: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(runtime_digest_file(codex_home)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    // Only trust the record if it is about the image this build actually uses.
    if v["image"].as_str().unwrap_or_default() != RUNTIME_REGISTRY_IMAGE {
        return None;
    }
    v["digest"].as_str().map(|s| s.to_string())
}

/// Freshness of the local runtime image against the registry.
#[derive(Serialize, Default)]
pub struct RuntimeImageStatus {
    /// The registry image this build pulls from, empty when it always builds.
    pub image: String,
    /// Digest of the manifest we pulled, empty when never pulled by this app.
    pub local: String,
    /// What the registry serves now, empty when the check could not run.
    pub remote: String,
    pub up_to_date: bool,
    /// True when a pull would bring something newer (or the local image was not
    /// pulled by us, so its contents are unknown).
    pub update_available: bool,
    pub error: String,
}

/// Compare the local runtime image with the registry.
///
/// An unanswerable check (offline, registry down) reports the error and claims
/// NOTHING: the caller must not read it as either fresh or stale.
async fn runtime_image_status(codex_home: &std::path::Path) -> RuntimeImageStatus {
    let mut out = RuntimeImageStatus {
        image: RUNTIME_REGISTRY_IMAGE.to_string(),
        local: recorded_runtime_digest(codex_home).unwrap_or_default(),
        ..Default::default()
    };
    if RUNTIME_REGISTRY_IMAGE.is_empty() {
        out.up_to_date = true; // this build always builds locally; nothing to compare
        return out;
    }
    if !runtime_image_exists().await {
        out.update_available = true; // nothing local at all
        return out;
    }
    match runtime::remote_digest(RUNTIME_REGISTRY_IMAGE).await {
        Some(remote) => {
            out.update_available = out.local.is_empty() || out.local != remote;
            out.up_to_date = !out.update_available;
            out.remote = remote;
        }
        None => out.error = "could not read the registry manifest".into(),
    }
    out
}

/// Ensure the runtime image exists locally: pull the prebuilt image with a
/// progress bar when a registry image is configured, else build it locally.
/// (Progress is emitted on `hacksor://runtime`.)
async fn provision_runtime_image(state: &State<'_, AppState>, app: &AppHandle) -> Result<(), String> {
    if !RUNTIME_REGISTRY_IMAGE.is_empty() {
        match pull_runtime_image(app, &state.codex_home).await {
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
    if let Some(reason) = runtime::docker_unavailable_reason().await {
        return Err(reason);
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

/// Is the local runtime image the one the registry serves? For Settings.
#[tauri::command]
pub async fn runtime_image_check(state: State<'_, AppState>) -> Result<RuntimeImageStatus, String> {
    Ok(runtime_image_status(&state.codex_home).await)
}

/// Pull the newest runtime image on demand and drop the old container so the
/// next start uses it.
#[tauri::command]
pub async fn pull_runtime_update(state: State<'_, AppState>, app: AppHandle) -> Result<(), String> {
    if RUNTIME_REGISTRY_IMAGE.is_empty() {
        return Err("This build has no registry image configured; it builds the runtime locally.".into());
    }
    pull_runtime_image(&app, &state.codex_home).await?;
    runtime::remove_container(RUNTIME_CONTAINER).await;
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
    if let Some(reason) = runtime::docker_unavailable_reason().await {
        return Err(reason);
    }
    // Is there anything to do at all? This runs on EVERY message and every
    // chat switch (ensure_harness calls it), and it used to announce
    // "Starting runtime container…" then "Runtime ready." unconditionally — so
    // the banner flashed on a container and proxy that had been up for hours.
    // The steps below are all idempotent and stay in place; only the
    // announcements are now conditional on work actually being needed.
    let already_up = runtime_container_running().await && opencodex_up().await;
    if !runtime_image_exists().await {
        provision_runtime_image(state, app).await?;
    } else {
        // The image exists, but the registry may have moved on. Hacksor used to
        // check existence ONLY, so anyone holding an old image silently kept it
        // forever and missed every runtime fix.
        //
        // The refresh runs in the BACKGROUND and the session continues on the
        // image already present. Pulling here would be a correctness bug of its
        // own: the image is ~19 GB and measured 1.5 MB/s from GHCR on this
        // machine, so awaiting it would hold the first chat hostage for hours
        // over an image that merely has a newer build of the same tools. The
        // new one is adopted when the container is next recreated (quit and
        // relaunch), which is also the only moment it can be swapped safely:
        // the running container cannot change image underneath a live turn.
        //
        // Failure is not staleness: an unanswerable check (offline, registry
        // down) leaves the working image alone and says nothing.
        let home = state.codex_home.clone();
        let app2 = app.clone();
        tauri::async_runtime::spawn(async move {
            let st = runtime_image_status(&home).await;
            if !st.update_available || !st.error.is_empty() {
                return;
            }
            let _ = app2.emit("hacksor://runtime", serde_json::json!({
                "phase": "pulling", "percent": 0,
                "message": "A newer runtime image is available — downloading it in the background…"
            }));
            match pull_runtime_image(&app2, &home).await {
                Ok(()) => {
                    let _ = app2.emit("hacksor://runtime", serde_json::json!({
                        "phase": "ready",
                        "message": "Newer runtime image downloaded — it is used next time you start Hacksor."
                    }));
                }
                Err(e) => {
                    // Keep the working image; just say why the refresh failed.
                    let _ = app2.emit("hacksor://runtime", serde_json::json!({
                        "phase": "ready",
                        "message": format!("Could not download the newer runtime image ({e}); continuing with the current one.")
                    }));
                }
            }
        });
    }
    if !already_up {
        let _ = app.emit("hacksor://runtime", serde_json::json!({ "phase": "starting", "message": "Starting runtime container…" }));
    }
    // On-demand (default): don't pre-start the heavy proxy/browser — the agent's
    // `hacksor-proxy start` and the patched camofox-mcp client bring them up when
    // actually used, saving RAM. Always-on: pre-start both to avoid warm-up.
    let (always_on, narrow, work) = {
        let s = state.settings.lock().await;
        (s.services_mode == "always_on", s.limit_runtime_mount, s.working_dir.clone())
    };
    start_runtime(&state.codex_home, always_on, narrow, &work).await?;
    if always_on {
        autostart_proxy_docker(&state.codex_home).await;
    } else {
        prepare_proxy_docker(&state.codex_home).await; // write CLI + wrapper so the agent can start it
    }
    ensure_opencodex(state).await?;
    // Only resolve a banner we actually raised: a silent no-op must stay silent.
    if !already_up {
        let _ = app.emit("hacksor://runtime", serde_json::json!({ "phase": "ready", "message": "Runtime ready." }));
    }
    Ok(())
}

/// Front-end entry point: prepare the runtime up front (startup / runtime switch)
/// so all the Docker download+build+start happens before the first message.
#[tauri::command]
pub async fn prepare_runtime(state: State<'_, AppState>, app: AppHandle) -> Result<(), String> {
    ensure_runtime_ready(&state, &app).await
}

/// True only on REAL rootful Linux — not Docker Desktop, not rootless Docker.
///
/// Two capabilities hinge on exactly this: sharing the host network namespace,
/// and running the container as the host uid:gid. Docker Desktop runs a VM (no
/// host networking; ownership already mapped). Rootless Docker's "host" namespace
/// is RootlessKit's rather than the machine's — a port bound inside is invisible
/// at the host's 127.0.0.1 — and container-root is already mapped to the host
/// user. Both of those cases publish ports and bridge them instead (see the socat
/// bridge in `ensure_opencodex`) and run as container root.
fn rootful_linux() -> bool {
    platform::IS_LINUX && !runtime::docker_rootless()
}

/// The `-u uid:gid` for `docker exec`/`run`, or `None` to run as container root.
///
/// Only rootful Linux needs it (there container-root would write root-owned files
/// into the bind-mounted home). Under rootless Docker passing a uid is actively
/// wrong: container-root already maps to the host user, while an explicit uid
/// lands files under an unrelated subuid.
fn exec_user() -> Option<String> {
    if rootful_linux() {
        platform::docker_exec_user()
    } else {
        None
    }
}

/// Start (or reuse) the runtime container. On rootful Linux it shares the host
/// network and runs as the host uid:gid (mounting $HOME at its real path). On
/// macOS / Windows (Docker Desktop VM) and under rootless Docker it publishes
/// ports instead and runs as root (ownership is mapped for us).
/// What to bind-mount into the runtime.
///
/// Normally the whole home directory, which is what an engagement usually
/// needs. When the mount is narrowed, only the working directory and Hacksor's
/// own codex home go in — the codex home is NOT optional: the harness reads its
/// config, skills and rollouts from there, and it lives outside any working
/// directory, so mounting the working directory alone would break the agent
/// rather than contain it.
///
/// Both are mounted at their real host paths so every path the agent is told
/// about resolves the same inside and out.
fn runtime_binds(codex_home: &std::path::Path, working_dir: &str, narrow: bool) -> (String, String, Vec<String>) {
    let (host_home, container_home) = platform::home_mount();
    if !narrow {
        return (host_home, container_home, vec![]);
    }
    let work = working_dir.trim();
    if work.is_empty() || !std::path::Path::new(work).is_dir() {
        // Nothing sane to narrow to; the whole home is better than no mount.
        return (host_home, container_home, vec![]);
    }
    let home = platform::to_container_path(codex_home);
    let work_c = platform::to_container_path(std::path::Path::new(work));
    (work.to_string(), work_c, vec![format!("{}:{}", codex_home.display(), home)])
}

async fn start_runtime(
    codex_home: &std::path::Path,
    services_always_on: bool,
    narrow_mount: bool,
    working_dir: &str,
) -> Result<(), String> {
    if let Some(reason) = runtime::docker_unavailable_reason().await {
        return Err(reason);
    }
    if !runtime_image_exists().await {
        return Err("Runtime image not available yet — it downloads/builds on first use.".into());
    }
    if !runtime_container_running().await {
        let (host_home, container_home, extra_binds) =
            runtime_binds(codex_home, working_dir, narrow_mount);
        let spec = runtime::RunSpec {
            image: RUNTIME_IMAGE.to_string(),
            name: RUNTIME_CONTAINER.to_string(),
            host_home,
            container_home,
            extra_binds,
            host_net: rootful_linux(),
            tun: runtime::tun_present(),
            // Without host networking (Docker Desktop VM, or rootless Docker's
            // isolated namespace) publish the in-container ocx + intercepting-proxy
            // ports so the host app reaches them at 127.0.0.1.
            publish_ports: if rootful_linux() { vec![] } else { vec![models::OPENCODEX_PORT, PROXY_PORT] },
        };
        runtime::create_and_start(&spec).await?;
        // Linux only: register the host uid/gid in the container passwd + grant
        // passwordless sudo, so codex/ocx run as the host user (no root-owned
        // files) yet can still sudo. On macOS/Windows we run as root (Docker
        // Desktop maps ownership), so this is unnecessary.
        if let Some(user) = exec_user() {
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
        // The image pre-fetches the ~1.3 GB Camoufox engine at BUILD time
        // (`camofox-browser fetch` in the Dockerfile), which runs as root and
        // lands in `/root/.cache/camoufox`. At RUNTIME, `$HOME` is the
        // bind-mounted user home (identity on Linux/macOS, `/hacksorhome` on
        // Windows) — never `/root` — so that cache is invisible to the running
        // container and every fresh install fails Camoufox's own version check
        // ("installed Camoufox version could not be determined") instead of
        // silently downloading. Seed the runtime cache from the build-time one
        // once per fresh container so the prefetch is actually used. Detached
        // and best-effort: if it's still copying (or missing) when the agent
        // first opens a browser tab, camofox-browser falls back to its normal
        // on-demand download — same as before this seed step existed.
        let _ = runtime::exec_detached(
            RUNTIME_CONTAINER,
            vec![
                "sh".into(),
                "-c".into(),
                "[ -f \"$HOME/.cache/camoufox/version.json\" ] || { mkdir -p \"$HOME/.cache\" && cp -r /root/.cache/camoufox \"$HOME/.cache/camoufox\" 2>/dev/null; }".into(),
            ],
            exec_user(),
        )
        .await;

        // Self-heal camofox-browser's native dependencies.
        //
        // npm 12 refuses to run install scripts unless the package is approved,
        // and only WARNS about the ones it skipped. camofox-browser needs
        // better-sqlite3 (`prebuild-install || node-gyp rebuild`) and argon2
        // (`node-gyp-build`), so any image built with npm 12 ships them with no
        // compiled addon: the first browser call dies on "Could not locate the
        // bindings file" and the whole Camoufox feature is unusable.
        //
        // The Dockerfile now builds them, but published images predating that
        // fix are already broken, and the fix lands in a container-local path
        // that a fresh container loses. So repair it here too: only when the
        // addon is actually missing (a no-op on a good image), and detached so
        // a slow compile never blocks the first chat.
        let _ = runtime::exec_detached(
            RUNTIME_CONTAINER,
            vec![
                "sh".into(),
                "-c".into(),
                "cd /usr/local/lib/node_modules/camofox-browser 2>/dev/null || exit 0; \
                 [ -f node_modules/better-sqlite3/build/Release/better_sqlite3.node ] && exit 0; \
                 npm install-scripts approve better-sqlite3 argon2 --no-allow-scripts-pin >/tmp/hacksor-camofox-deps.log 2>&1; \
                 npm rebuild better-sqlite3 argon2 >>/tmp/hacksor-camofox-deps.log 2>&1"
                    .into(),
            ],
            Some("0".into()),
        )
        .await;
    }
    let _ = codex_home; // the home mount already covers codex-home + working dirs
    // Refresh vuln intel in the background so each session has fresh CVE data.
    let _ = runtime::exec_detached(
        RUNTIME_CONTAINER,
        vec!["sh".into(), "-c".into(), "nuclei -update-templates -silent >/dev/null 2>&1; searchsploit -u >/dev/null 2>&1".into()],
        exec_user(),
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
            exec_user(),
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
            exec_user(),
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

/// `hacksor-identity`: throwaway inboxes for sign-ups during an engagement.
/// Shipped like the proxy CLI — written into the codex home and put on the
/// container's PATH — so a runtime image rebuild is not needed to update it.
const IDENTITY_CLI: &str = include_str!("../../docker/hacksor-identity");

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
/// Write `hacksor-identity` into the codex home and put it on the container's
/// PATH. The identities file lives beside it, so the addresses created for an
/// engagement survive a container restart.
async fn install_identity_cli(codex_home: &std::path::Path) {
    let dir = codex_home.join("tools");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let script = dir.join("hacksor-identity");
    let store = dir.join("identities.json");
    if std::fs::write(&script, IDENTITY_CLI).is_err() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755));
    }
    let wrapper = format!(
        "#!/bin/sh\nexec env HACKSOR_IDENTITY_STORE='{store}' python3 '{script}' \"$@\"\n",
        store = platform::to_container_path(&store),
        script = platform::to_container_path(&script),
    );
    let cmd = format!(
        "cat > /usr/local/bin/hacksor-identity <<'HXEOF'\n{wrapper}HXEOF\nchmod +x /usr/local/bin/hacksor-identity"
    );
    let _ = runtime::exec_output(
        RUNTIME_CONTAINER,
        vec!["sh".into(), "-c".into(), cmd],
        Some("0".into()),
        None,
    )
    .await;
}

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
    install_identity_cli(codex_home).await; // `hacksor-identity` likewise
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
    let _ = runtime::exec_detached(RUNTIME_CONTAINER, proxy_start_cmd(&script, &log, &rules), exec_user()).await;
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
    if let Some(reason) = runtime::docker_unavailable_reason().await {
        return Err(reason);
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
        extra_binds: vec![],
        host_net: rootful_linux(),
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

// ---- session ledger ----

/// Where a session records what it learns.
#[derive(Serialize, Default)]
pub struct SessionLedger {
    /// Root of the engagement directory, as the AGENT sees it.
    pub dir: String,
    /// The same path on the host, for the UI.
    pub host_dir: String,
    /// The findings store `hacksor-findings --file` writes to.
    pub findings: String,
    /// The running narrative the agent keeps current.
    pub notes: String,
    /// Where the generated report lands.
    pub report: String,
    /// False when the directory already existed, so a resumed session reuses it.
    pub created: bool,
}

/// Ask the model what this engagement is against.
///
/// The first message is not a target: "scan 10.0.0.5 and tell me what is
/// running" names one, "hey how are u" does not. Taking the opening words
/// produced directories like `hey-how-are-u`, so the naming is a judgement and
/// the model makes it — including the judgement that there is nothing to name.
#[tauri::command]
pub async fn suggest_target(state: State<'_, AppState>, args: TitleArgs) -> Result<String, String> {
    let sys = "Decide whether this first message STARTS A PENETRATION TEST, and if so name its target.\n\
\n\
Reply with ONLY the target — a hostname, domain, IP, application or organisation name. No path, no scheme, \
no prose, no quotes, no explanation.\n\
\n\
Reply with exactly `none` unless the user is asking you to TEST, ATTACK, ASSESS, SCAN, EXPLOIT or AUDIT a \
specific target of their own. `none` is the right answer for all of these, even when a host or product is \
mentioned by name:\n\
- greetings, small talk, questions about you or your tools;\n\
- how something works, what a vulnerability class is, how to use a tool;\n\
- writing, reviewing, explaining or debugging code;\n\
- a one-off command or lookup with no engagement behind it;\n\
- anything where the user is asking ABOUT a thing rather than asking you to go after it.\n\
\n\
Naming a host is not enough on its own. When in doubt, answer `none`: a directory that should not exist is \
worse than one created a message later.";
    let user = format!("First message:\n\n{}", truncate(args.text.trim(), 1500));
    let Some(raw) = judge_call(&state, &args.provider, &args.model, sys, &user).await else {
        return Ok(String::new());
    };
    Ok(clean_target(&raw))
}

/// Reduce the model's answer to a target, or "" for "this is not an engagement".
fn clean_target(raw: &str) -> String {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let line = line.trim_matches(|c| c == '"' || c == '\'' || c == '`' || c == '.').trim();
    let lower = line.to_lowercase();
    // The model is told to answer "none"; it sometimes says it in a sentence.
    if lower.is_empty()
        || lower == "none"
        || lower.starts_with("none")
        || lower.contains("no target")
        || lower.contains("not an engagement")
    {
        return String::new();
    }
    // A target is a name, not a sentence: anything long is the model ignoring
    // the instruction, and a bad directory name is worse than none.
    if line.split_whitespace().count() > 5 || line.chars().count() > 60 {
        return String::new();
    }
    line.to_string()
}

/// An engagement directory already under `root` for this target, if one exists.
///
/// Re-testing the same target, branching a chat, or simply starting again must
/// land in the SAME directory: a second `host-2` beside `host` splits the
/// record of one engagement in half. Only directories that actually look like
/// engagements (they carry the notes file) are candidates, so an unrelated
/// folder with a similar name is never adopted.
fn find_existing_engagement(root: &std::path::Path, slug: &str) -> Option<std::path::PathBuf> {
    let key = |s: &str| -> String {
        s.trim_start_matches("www.")
            .split('.')
            .next()
            .unwrap_or(s)
            .replace(['-', '_'], "")
            .to_lowercase()
    };
    let want = key(slug);
    if want.is_empty() {
        return None;
    }
    let mut best: Option<std::path::PathBuf> = None;
    for e in std::fs::read_dir(root).ok()?.flatten() {
        let p = e.path();
        if !p.is_dir() || !p.join("ENGAGEMENT_NOTES.md").is_file() {
            continue;
        }
        let name = p.file_name()?.to_string_lossy().to_string();
        if name.eq_ignore_ascii_case(slug) {
            return Some(p); // exact beats everything
        }
        if key(&name) == want {
            best = Some(p);
        }
    }
    best
}

/// A directory name for a target.
///
/// Targets arrive as whatever the user typed: a URL, a host, a company name, a
/// sentence. Strip a scheme and a path, keep what a person would recognise, and
/// never produce something that escapes the engagement root.
fn target_slug(raw: &str) -> String {
    let mut t = raw.trim().to_lowercase();
    for p in ["https://", "http://", "ssh://", "ftp://"] {
        if let Some(rest) = t.strip_prefix(p) {
            t = rest.to_string();
        }
    }
    // A URL's path, query or port is noise in a directory name.
    t = t.split(['/', '?', '#']).next().unwrap_or("").to_string();
    if let Some((host, _port)) = t.rsplit_once(':') {
        if t.rsplit_once(':').map(|(_, p)| p.chars().all(|c| c.is_ascii_digit())).unwrap_or(false) {
            t = host.to_string();
        }
    }
    let mut out = String::new();
    let mut last_dash = false;
    for c in t.chars() {
        if c.is_ascii_alphanumeric() || c == '.' {
            out.push(c);
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    let out = out.trim_matches(['-', '.']).to_string();
    let out: String = out.chars().take(48).collect();
    let out = out.trim_matches(['-', '.']).to_string();
    if out.is_empty() {
        return format!("session-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S"));
    }
    // Windows refuses to create a file or directory whose name — up to the
    // first dot — is a reserved device name, so `nul` and `nul.example.com`
    // are both rejected there. The same engagement must land in the same
    // directory on every platform, so rename it everywhere, not only on
    // Windows.
    const RESERVED: [&str; 22] = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7",
        "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    // It is the stem that must change: `nul.example.com-target` still begins
    // with `nul` up to the first dot, so Windows would reject it too.
    let (stem, rest) = out.split_once('.').unwrap_or((out.as_str(), ""));
    if RESERVED.contains(&stem) {
        return if rest.is_empty() {
            format!("{stem}-target")
        } else {
            format!("{stem}-target.{rest}")
        };
    }
    out
}

/// The files a fresh engagement directory starts with.
///
/// The layout follows a real engagement tree: evidence under `artifacts/` by
/// workstream, what ships under `deliverables/`, what must NOT ship under
/// `_quarantine_not_in_delivery/`, and two living documents at the root. The
/// notes file is the one that matters for continuity: it is what the agent
/// re-reads after a compaction, when its own context no longer holds the
/// engagement.
fn ledger_skeleton(target: &str) -> Vec<(&'static str, String)> {
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    vec![
        (
            "ENGAGEMENT_NOTES.md",
            format!(
                "# Engagement notes: {target}\n\n\
                 Started {today}. This file is the engagement's memory. The context window is\n\
                 not: it compacts, and everything not written down here is gone when it does.\n\
                 Keep it current as you work, not at the end.\n\n\
                 ## Scope\n\n\
                 <!-- in-scope hosts, apps, accounts, and the explicit boundaries -->\n\n\
                 ## Current state\n\n\
                 <!-- REWRITE this section as it changes: what is established, what is being\n\
                 tested right now, and the single next action. First thing to read on resume. -->\n\n\
                 ## Access and credentials\n\n\
                 <!-- what access exists and how it was obtained (reference the ledger entry) -->\n\n\
                 ## Map\n\n\
                 <!-- hosts, endpoints, parameters, versions worth remembering -->\n\n\
                 ## Open leads\n\n\
                 <!-- what is worth trying next, each with why -->\n\n\
                 ## Ruled out\n\n\
                 <!-- what was tested and did NOT work, with the boundary that stopped it.\n\
                 This prevents re-running the same dead end after a compaction. -->\n"
            ),
        ),
        (
            "RESIDUAL_STATE.md",
            format!(
                "# Residual state: {target}\n\n\
                 Everything created on the target that does not belong there: accounts,\n\
                 uploads, webhooks, registrations, files, scheduled jobs. Append on creation,\n\
                 not from memory at the end.\n\n\
                 | created | what | where | removed |\n\
                 |---|---|---|---|\n"
            ),
        ),
        (
            "ledger/index.md",
            format!(
                "# Ledger index: {target}\n\n\
                 One line per entry, newest last. The entries themselves live in\n\
                 `ledger/entries/` and are read on demand: this index is what stays cheap\n\
                 enough to keep in context.\n\n\
                 | id | kind | status | one line |\n\
                 |---|---|---|---|\n"
            ),
        ),
        (
            "README.md",
            format!(
                "# {target}\n\n\
                 Engagement workspace, created by Hacksor on {today}.\n\n\
                 | path | what goes here |\n\
                 |---|---|\n\
                 | `ENGAGEMENT_NOTES.md` | scope, current state, map, open leads, ruled out |\n\
                 | `RESIDUAL_STATE.md` | anything created on the target that must be removed |\n\
                 | `findings.jsonl` | the findings store (`hacksor-findings --file`) |\n\
                 | `ledger/index.md` | one line per ledger entry |\n\
                 | `ledger/entries/` | one card per finding, lead, negative, decision or blocker |\n\
                 | `artifacts/<workstream>/` | evidence: scans, captures, PoCs, decompiles |\n\
                 | `deliverables/` | the report and anything else handed over |\n\
                 | `_quarantine_not_in_delivery/` | kept for the record, never shipped |\n"
            ),
        ),
    ]
}

/// Lay out an engagement directory. Never overwrites: a resumed engagement
/// keeps every note and finding it already wrote.
fn create_ledger_tree(dir: &std::path::Path, slug: &str) -> std::io::Result<()> {
    for sub in ["artifacts", "deliverables", "_quarantine_not_in_delivery", "ledger/entries"] {
        std::fs::create_dir_all(dir.join(sub))?;
    }
    for (name, body) in ledger_skeleton(slug) {
        let f = dir.join(name);
        if !f.exists() {
            std::fs::write(&f, body)?;
        }
    }
    let findings = dir.join("findings.jsonl");
    if !findings.exists() {
        std::fs::write(&findings, "")?;
    }
    Ok(())
}

/// Recover which engagement directory a thread belongs to, by reading its
/// rollout.
///
/// The link normally lives in the chat record, but chats created before that
/// field was persisted have no directory recorded and would never show the
/// engagement view again — the work exists on disk with nothing pointing at
/// it. The preamble injected at the start of an engagement names the path, so
/// the rollout is an authoritative record of it.
#[tauri::command]
pub async fn ledger_recover(
    state: State<'_, AppState>,
    thread_id: String,
) -> Result<Option<SessionLedger>, String> {
    let Some(path) = find_rollout(&state.codex_home, &thread_id) else { return Ok(None) };
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    // The rollout records paths as the AGENT saw them, which under Docker are
    // container paths. Map back before touching the filesystem.
    let as_host = |d: &str| std::path::PathBuf::from(crate::platform::from_container_path(d));
    let is_engagement = |p: &std::path::Path| p.join("ENGAGEMENT_NOTES.md").is_file();

    let direct = engagement_dir_in_rollout(&text).map(|d| as_host(&d));
    let host_dir = match direct {
        // The preamble or the cwd named it outright.
        Some(d) if is_engagement(&d) => d,
        // The cwd is often the directory engagements live UNDER rather than an
        // engagement itself: the agent opens the chat in ~/sec and cd's into
        // the target per command. Look one level down and take the engagement
        // the thread actually worked in.
        Some(parent) => match engagement_worked_in(&parent, &text) {
            Some(d) => d,
            None => return Ok(None),
        },
        None => return Ok(None),
    };
    let docker = docker_runtime(&state).await;
    let seen = |p: &std::path::Path| -> String {
        if docker { crate::platform::to_container_path(p) } else { p.to_string_lossy().into_owned() }
    };
    Ok(Some(SessionLedger {
        dir: seen(&host_dir),
        host_dir: host_dir.to_string_lossy().into_owned(),
        findings: seen(&host_dir.join("findings.jsonl")),
        notes: seen(&host_dir.join("ENGAGEMENT_NOTES.md")),
        report: seen(&host_dir.join("deliverables").join("report.md")),
        created: false,
    }))
}

/// Find the engagement directory a rollout belongs to.
///
/// Two signals, best first:
///
/// 1. The preamble line, when the session got one — it names the directory
///    outright.
/// 2. The thread's recorded `cwd`. A session opened on an engagement runs in
///    it, so the working directory IS the engagement. This is the one that
///    actually fires in practice: threads whose preamble was skipped (it rides
///    on the second message, and a turn can end before that) still record a
///    cwd on the very first line.
///
/// Either way the caller checks the directory really is an engagement before
/// offering it, so a plain cwd on an ordinary chat is harmless.
fn engagement_dir_in_rollout(text: &str) -> Option<String> {
    const MARK: &str = "This session's engagement directory is ";
    // Last match wins: a thread that moved to another engagement should
    // recover the one it is actually on.
    if let Some(at) = text.rfind(MARK).map(|i| i + MARK.len()) {
        let rest = &text[at..];
        let end = rest
            .find(". It is the record")
            .or_else(|| rest.find("\\n"))
            .unwrap_or(rest.len());
        let dir = rest[..end].trim().trim_end_matches('.').trim();
        if !dir.is_empty() {
            return Some(dir.to_string());
        }
    }
    // The session meta is the first line of the rollout.
    let first = text.lines().next()?;
    let v: serde_json::Value = serde_json::from_str(first).ok()?;
    let cwd = v
        .pointer("/payload/cwd")
        .or_else(|| v.pointer("/cwd"))
        .and_then(|c| c.as_str())?;
    (!cwd.is_empty()).then(|| cwd.to_string())
}

/// Of the engagements directly under `root`, the one this rollout worked in.
///
/// Picked by how often each directory's name occurs in the thread, so a
/// session that touched one engagement in passing does not outvote the one it
/// actually ran. Ties and zero mentions return nothing rather than a guess:
/// opening the wrong engagement is worse than offering no button.
fn engagement_worked_in(root: &std::path::Path, text: &str) -> Option<std::path::PathBuf> {
    let mut best: Option<(usize, std::path::PathBuf)> = None;
    let mut tied = false;
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let p = entry.path();
        if !p.join("ENGAGEMENT_NOTES.md").is_file() {
            continue;
        }
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else { continue };
        let hits = text.matches(name).count();
        if hits == 0 {
            continue;
        }
        match &best {
            Some((n, _)) if hits < *n => {}
            Some((n, _)) if hits == *n => tied = true,
            _ => {
                best = Some((hits, p));
                tied = false;
            }
        }
    }
    if tied { None } else { best.map(|(_, p)| p) }
}

/// Create (or adopt) the engagement directory for a session.
///
/// Idempotent: an existing directory is reused and no file is overwritten, so
/// resuming a session keeps everything already recorded.
#[tauri::command]
pub async fn ledger_init(
    state: State<'_, AppState>,
    working_dir: String,
    target: String,
) -> Result<SessionLedger, String> {
    let docker = docker_runtime(&state).await;
    let root = std::path::PathBuf::from(if working_dir.trim().is_empty() {
        state.settings.lock().await.working_dir.clone()
    } else {
        working_dir
    });
    let slug = target_slug(&target);
    let dir = find_existing_engagement(&root, &slug).unwrap_or_else(|| root.join(&slug));
    let created = !dir.exists();
    create_ledger_tree(&dir, &slug).map_err(|e| format!("{}: {e}", dir.display()))?;
    let findings = dir.join("findings.jsonl");
    // The agent sees container paths in Docker mode; the UI shows host paths.
    let seen = |p: &std::path::Path| -> String {
        if docker { crate::platform::to_container_path(p) } else { p.to_string_lossy().into_owned() }
    };
    Ok(SessionLedger {
        dir: seen(&dir),
        host_dir: dir.to_string_lossy().into_owned(),
        findings: seen(&findings),
        notes: seen(&dir.join("ENGAGEMENT_NOTES.md")),
        report: seen(&dir.join("deliverables").join("report.md")),
        created,
    })
}

// ---- findings notes (shared with the agent via codex_home/notes) ----

/// What a purge actually removed, so the UI can say it plainly.
#[derive(serde::Serialize, Default)]
pub struct PurgeReport {
    pub rollout: bool,
    pub engagement: bool,
    pub notes: bool,
    /// Anything that was asked for and could not be removed.
    pub errors: Vec<String>,
}

/// Delete the traces of one engagement: the stored conversation, the
/// engagement directory, and the chat's note.
///
/// Archiving only hides a chat. When an engagement ends — or a client asks for
/// their data gone — the transcript on disk, the reports and the note have to
/// go too, and hunting them down by hand is how one gets left behind. Each part
/// is opt-in: the caller says what to remove and this reports what it did.
///
/// A delete here is not recoverable, so the paths are checked before they are
/// touched: the engagement directory must be a directory that actually carries
/// `ENGAGEMENT_NOTES.md`, which keeps a mistyped or stale path from taking an
/// unrelated folder with it.
#[tauri::command]
pub fn purge_thread(
    state: State<'_, AppState>,
    thread_id: Option<String>,
    engagement_dir: Option<String>,
    notes_path: Option<String>,
    delete_rollout: bool,
    delete_engagement: bool,
    delete_notes: bool,
) -> Result<PurgeReport, String> {
    let mut out = PurgeReport::default();

    if delete_rollout {
        match thread_id.as_deref().filter(|t| !t.is_empty()) {
            None => out.errors.push("this chat has no stored conversation yet".into()),
            Some(tid) => {
                match find_rollout(&state.codex_home, tid) {
                    None => out.errors.push("no stored conversation found on disk".into()),
                    Some(path) => match std::fs::remove_file(&path) {
                        Ok(()) => out.rollout = true,
                        Err(e) => out.errors.push(format!("conversation: {e}")),
                    },
                }
            }
        }
    }

    if delete_engagement {
        match engagement_dir.as_deref().filter(|d| !d.is_empty()) {
            None => out.errors.push("this chat has no engagement directory".into()),
            Some(dir) => {
                let path = PathBuf::from(dir);
                if !path.is_dir() {
                    out.errors.push("the engagement directory is already gone".into());
                } else if !path.join("ENGAGEMENT_NOTES.md").is_file() {
                    // Refuse rather than recurse into something that was never ours.
                    out.errors.push(
                        "refused: that directory does not look like an engagement (no ENGAGEMENT_NOTES.md)".into(),
                    );
                } else {
                    match std::fs::remove_dir_all(&path) {
                        Ok(()) => out.engagement = true,
                        Err(e) => out.errors.push(format!("engagement directory: {e}")),
                    }
                }
            }
        }
    }

    if delete_notes {
        // The chat's note is the ledger's ENGAGEMENT_NOTES.md. Deleting the
        // whole directory covers it; this is for keeping the evidence while
        // dropping the running narrative.
        match notes_path.as_deref().filter(|n| !n.is_empty()) {
            None => out.errors.push("this chat has no engagement notes".into()),
            Some(raw) => {
                let path = PathBuf::from(raw);
                if path.file_name().and_then(|n| n.to_str()) != Some("ENGAGEMENT_NOTES.md") {
                    out.errors.push("refused: that is not an engagement notes file".into());
                } else if !path.is_file() {
                    out.errors.push("the engagement notes are already gone".into());
                } else {
                    match std::fs::remove_file(&path) {
                        Ok(()) => out.notes = true,
                        Err(e) => out.errors.push(format!("notes: {e}")),
                    }
                }
            }
        }
    }

    Ok(out)
}

/// What the egress looks like right now.
#[derive(serde::Serialize, Default, Clone)]
pub struct VpnStatus {
    /// The default route leaves through a tunnel interface.
    pub tunnel: bool,
    /// Name of the interface carrying the default route (en0, utun3, …).
    pub iface: String,
    /// Public address as seen from outside, "" when the lookup failed.
    pub ip: String,
    pub country: String,
    pub isp: String,
    /// The lookup service flags the address as a proxy/VPN exit.
    pub proxy: bool,
    /// The address belongs to a hosting provider, which is what a cheap VPS
    /// exit looks like from outside.
    pub hosting: bool,
    /// True when nothing suggests the traffic is leaving through anything but
    /// the plain home connection.
    pub exposed: bool,
    /// Set when the public lookup could not be done; `exposed` then rests on
    /// the local check alone.
    pub lookup_error: String,
}

/// Which interface carries the default route.
///
/// Presence of a tunnel interface proves nothing: macOS keeps utun0-utun5 up
/// for its own services whether or not a VPN is connected. What matters is
/// whether the DEFAULT ROUTE goes through one.
fn default_route_iface() -> String {
    #[cfg(target_os = "macos")]
    let out = std::process::Command::new("route").args(["-n", "get", "default"]).output();
    #[cfg(target_os = "linux")]
    let out = std::process::Command::new("ip").args(["route", "show", "default"]).output();
    #[cfg(target_os = "windows")]
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command",
               "(Get-NetRoute -DestinationPrefix '0.0.0.0/0' | Sort-Object RouteMetric | Select-Object -First 1).InterfaceAlias"])
        .output();

    let Ok(o) = out else { return String::new() };
    let text = String::from_utf8_lossy(&o.stdout).to_string();

    #[cfg(target_os = "macos")]
    {
        for line in text.lines() {
            if let Some(v) = line.trim().strip_prefix("interface:") {
                return v.trim().to_string();
            }
        }
        String::new()
    }
    #[cfg(target_os = "linux")]
    {
        // "default via 10.0.0.1 dev wg0 proto static"
        let mut it = text.split_whitespace();
        while let Some(tok) = it.next() {
            if tok == "dev" {
                return it.next().unwrap_or("").to_string();
            }
        }
        String::new()
    }
    #[cfg(target_os = "windows")]
    {
        text.trim().to_string()
    }
}

/// Does this interface name look like a tunnel?
fn is_tunnel_iface(name: &str) -> bool {
    let n = name.trim().to_lowercase();
    if n.is_empty() {
        return false;
    }
    const PREFIXES: [&str; 7] = ["utun", "tun", "tap", "wg", "ipsec", "ppp", "nordlynx"];
    if PREFIXES.iter().any(|p| n.starts_with(p)) {
        return true;
    }
    // Windows reports friendly names rather than device names.
    ["vpn", "wireguard", "openvpn", "tailscale", "mullvad", "proton"]
        .iter()
        .any(|p| n.contains(p))
}

/// Is the agent about to talk to the target from the operator's own address?
///
/// Two independent signals, because either alone misleads: the default route's
/// interface (local, instant, but blind to a proxy set per-application) and a
/// public lookup (authoritative about what the target would see, but needs the
/// network and tells a third party this address asked).
/// The LOCAL half only: which interface carries the default route.
///
/// Instant — no network. The gate asks this first so the warning can appear the
/// moment Send is pressed; waiting on the public lookup put a multi-second
/// stall between the keystroke and the dialog.
#[tauri::command]
pub fn vpn_route() -> VpnStatus {
    let iface = default_route_iface();
    let tunnel = is_tunnel_iface(&iface);
    VpnStatus { tunnel, iface, exposed: !tunnel, ..Default::default() }
}

/// Cache the public lookup: a few sends in a row should not each wait on (and
/// re-tell) a third party. The egress rarely changes within a few minutes.
static IP_CACHE: Mutex<Option<(std::time::Instant, VpnStatus)>> = Mutex::const_new(None);
const IP_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

#[tauri::command]
pub async fn vpn_status() -> Result<VpnStatus, String> {
    let iface = default_route_iface();
    let tunnel = is_tunnel_iface(&iface);
    // Leaving through a tunnel is already the answer; do not tell a third
    // party about an address that is not even exposed.
    if tunnel {
        return Ok(VpnStatus { tunnel, iface, exposed: false, ..Default::default() });
    }
    if let Some((at, cached)) = IP_CACHE.lock().await.as_ref() {
        if at.elapsed() < IP_CACHE_TTL {
            let mut st = VpnStatus { tunnel, iface, ..cached.clone() };
            st.exposed = !(st.tunnel || st.proxy || st.hosting);
            return Ok(st);
        }
    }
    let mut st = VpnStatus { tunnel, iface, ..Default::default() };

    let url = "http://ip-api.com/json/?fields=status,message,country,isp,proxy,hosting,query";
    match reqwest::Client::new()
        .get(url)
        .timeout(std::time::Duration::from_secs(4))
        .send()
        .await
    {
        Ok(r) => match r.json::<serde_json::Value>().await {
            Ok(v) if v.get("status").and_then(|s| s.as_str()) == Some("success") => {
                st.ip = v.get("query").and_then(|x| x.as_str()).unwrap_or("").to_string();
                st.country = v.get("country").and_then(|x| x.as_str()).unwrap_or("").to_string();
                st.isp = v.get("isp").and_then(|x| x.as_str()).unwrap_or("").to_string();
                st.proxy = v.get("proxy").and_then(|x| x.as_bool()).unwrap_or(false);
                st.hosting = v.get("hosting").and_then(|x| x.as_bool()).unwrap_or(false);
            }
            Ok(v) => {
                st.lookup_error = v
                    .get("message")
                    .and_then(|x| x.as_str())
                    .unwrap_or("lookup refused")
                    .to_string();
            }
            Err(e) => st.lookup_error = e.to_string(),
        },
        Err(e) => st.lookup_error = e.to_string(),
    }

    st.exposed = !(st.tunnel || st.proxy || st.hosting);
    if st.lookup_error.is_empty() {
        *IP_CACHE.lock().await = Some((std::time::Instant::now(), st.clone()));
    }
    Ok(st)
}

/// One finding as `hacksor-findings` stores it (JSONL, one object per line).
#[derive(serde::Serialize, Default)]
pub struct Finding {
    pub id: String,
    pub title: String,
    pub severity: String,
    pub cvss: String,
    pub cvss_vector: String,
    pub owasp: String,
    pub url: String,
    pub description: String,
    pub evidence: String,
    pub remediation: String,
    pub status: String,
}

/// A file inside the engagement directory, for the browser.
#[derive(serde::Serialize)]
pub struct EngagementFile {
    /// Path relative to the engagement root, which is also the key used to read it.
    pub rel: String,
    pub size: u64,
    /// Seconds since the epoch, for sorting by recency.
    pub modified: u64,
    pub dir: bool,
}

/// Everything the UI needs to show one engagement at a glance.
#[derive(serde::Serialize, Default)]
pub struct EngagementView {
    pub dir: String,
    pub findings: Vec<Finding>,
    pub files: Vec<EngagementFile>,
    /// Present-and-readable markers for the documents the panel links to.
    pub has_notes: bool,
    pub has_report: bool,
    pub has_residual: bool,
    pub total_bytes: u64,
}

fn as_str(v: &serde_json::Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// Rank for sorting: the worst finding should be the first one read.
fn severity_rank(s: &str) -> u8 {
    match s.to_ascii_lowercase().as_str() {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
}

/// Resolve `rel` inside `root`, refusing anything that escapes it.
///
/// The panel passes paths that came from a listing, but a bug or a crafted
/// name must not turn a read of the engagement directory into a read of the
/// whole disk — so both sides are canonicalised and the result has to still
/// be under the root.
fn engagement_path(root: &std::path::Path, rel: &str) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let joined = root.join(rel).canonicalize().ok()?;
    joined.starts_with(&root).then_some(joined)
}

/// Walk the engagement directory, newest first, bounded so a 60k-file tree
/// cannot stall the UI.
fn walk_engagement(root: &std::path::Path, out: &mut Vec<EngagementFile>, total: &mut u64, budget: &mut usize) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if *budget == 0 {
            return;
        }
        let path = e.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            walk_engagement(&path, out, total, budget);
            continue;
        }
        *budget -= 1;
        *total += meta.len();
        out.push(EngagementFile {
            rel: String::new(), // filled by the caller, which knows the root
            size: meta.len(),
            modified: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
            dir: false,
        });
        // Store the path separately so `rel` can be computed once below.
        out.last_mut().unwrap().rel = path.to_string_lossy().into_owned();
    }
}

/// How many files the browser lists. An engagement can hold tens of thousands
/// (one here holds 60,798); the panel is for navigating the work, not for
/// mirroring the filesystem.
const ENGAGEMENT_FILE_CAP: usize = 4000;

#[tauri::command]
pub async fn engagement_view(dir: String) -> Result<EngagementView, String> {
    tokio::task::spawn_blocking(move || {
        let root = PathBuf::from(&dir);
        if !root.is_dir() {
            return Err(format!("{} is not a directory", root.display()));
        }
        let mut view = EngagementView { dir: dir.clone(), ..Default::default() };

        // Findings: JSONL, newest store wins, worst severity first.
        for name in ["findings.json", "findings.jsonl"] {
            let p = root.join(name);
            let Ok(text) = std::fs::read_to_string(&p) else { continue };
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                view.findings.push(Finding {
                    id: as_str(&v, "id"),
                    title: as_str(&v, "title"),
                    severity: as_str(&v, "severity"),
                    cvss: as_str(&v, "cvss"),
                    cvss_vector: as_str(&v, "cvss_vector"),
                    owasp: as_str(&v, "owasp"),
                    url: as_str(&v, "url"),
                    description: as_str(&v, "description"),
                    evidence: as_str(&v, "evidence"),
                    remediation: as_str(&v, "remediation"),
                    status: as_str(&v, "status"),
                });
            }
            if !view.findings.is_empty() {
                break;
            }
        }
        view.findings.sort_by_key(|f| severity_rank(&f.severity));

        view.has_notes = root.join("ENGAGEMENT_NOTES.md").is_file();
        view.has_report = root.join("deliverables/report.md").is_file();
        view.has_residual = root.join("RESIDUAL_STATE.md").is_file();

        let mut files = Vec::new();
        let mut budget = ENGAGEMENT_FILE_CAP;
        walk_engagement(&root, &mut files, &mut view.total_bytes, &mut budget);
        let prefix = format!("{}/", root.to_string_lossy());
        for f in &mut files {
            if let Some(stripped) = f.rel.strip_prefix(&prefix) {
                f.rel = stripped.to_string();
            }
        }
        files.sort_by(|a, b| b.modified.cmp(&a.modified));
        view.files = files;
        Ok(view)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Read one file from the engagement directory for preview.
/// Open the engagement directory, or one file in it, in the OS file manager.
///
/// The panel reads text; evidence is also packet captures, screenshots and
/// binaries it will not preview, and a report is often the thing you want to
/// hand to someone. Rather than reimplement a file manager, hand off to the
/// one the machine already has.
#[tauri::command]
pub async fn engagement_reveal(dir: String, rel: Option<String>) -> Result<(), String> {
    let root = PathBuf::from(&dir);
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    // A relative path still has to resolve inside the engagement: this opens
    // things with the desktop's own handlers, so it must not be steerable.
    let target = match rel.as_deref().filter(|r| !r.is_empty()) {
        Some(r) => engagement_path(&root, r)
            .ok_or_else(|| "that path is outside the engagement directory".to_string())?,
        None => root.canonicalize().map_err(|e| e.to_string())?,
    };
    platform::open_url(&target.to_string_lossy()).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn engagement_file(dir: String, rel: String) -> Result<String, String> {
    tokio::task::spawn_blocking(move || {
        let root = PathBuf::from(&dir);
        let Some(path) = engagement_path(&root, &rel) else {
            return Err("that path is outside the engagement directory".to_string());
        };
        let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
        // Artifacts include packet captures and binaries; the panel previews
        // text, and says so rather than hanging on a 200 MB file.
        const PREVIEW_MAX: u64 = 2 * 1024 * 1024;
        if meta.len() > PREVIEW_MAX {
            return Err(format!(
                "{} is {:.1} MB — too large to preview; open it from the engagement directory",
                rel,
                meta.len() as f64 / 1_048_576.0
            ));
        }
        let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
        if bytes.contains(&0) {
            return Err(format!("{rel} looks binary"));
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    })
    .await
    .map_err(|e| e.to_string())?
}

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
        crate::harness::LaunchMode::Docker { container: RUNTIME_CONTAINER.to_string(), user: exec_user() }
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
/// personality) plus the ported HackerAI persona, and — for the models that
/// ship one — that model's operating spec.
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
    fn phase_verdict_keeps_the_phase_unless_the_judge_is_clear() {
        // Clean verdicts, and the same wrapped in the prose models tend to add.
        let v = parse_phase_verdict(r#"{"phase":"exploit","reason":"confirmed SQLi on /login id param"}"#);
        assert_eq!(v.phase, "exploit");
        assert_eq!(v.reason, "confirmed SQLi on /login id param");
        assert_eq!(parse_phase_verdict("```json\n{\"phase\":\"recon\"}\n```").phase, "recon");
        // Anything unclear must NOT move the phase.
        for junk in [
            "",
            "recon",                              // bare word, no JSON
            "{}",                                 // no phase field
            r#"{"phase":"exploitation"}"#,        // not one of the two
            r#"{"phase":"EXPLOIT"}"#,             // wrong case
            r#"{"phase":null}"#,
            "{not json at all}",
        ] {
            assert_eq!(parse_phase_verdict(junk).phase, "unknown", "{junk:?}");
        }
    }

    #[test]
    fn models_available_parses_the_provider_test_count() {
        // The real shape, lowercased the way the caller passes it.
        assert_eq!(
            models_available_in("vercel-ai-gateway: connected\nconnected — 391 models available.\nlatency: 1233 ms"),
            Some(391)
        );
        assert_eq!(models_available_in("connected — 0 models available."), Some(0));
        // No count, or a non-numeric one: fall back to the generic message.
        assert_eq!(models_available_in("vercel-ai-gateway: connected"), None);
        assert_eq!(models_available_in("connected — no models available."), None);
    }

    #[test]
    fn transcript_slice_keeps_the_tail_but_all_the_prompts() {
        let mk = |i: usize, user: bool| {
            if user {
                serde_json::json!({"type":"userMessage","id":format!("t{i}"),"content":[{"type":"text","text":format!("prompt {i}")}]})
            } else {
                serde_json::json!({"type":"agentMessage","id":format!("t{i}"),"text":format!("answer {i}")})
            }
        };
        // 100 records, every 10th a user prompt (10 prompts, 8 of them trimmed).
        let items: Vec<_> = (0..100).map(|i| mk(i, i % 10 == 0)).collect();

        let t = slice_transcript(items.clone(), Some(20));
        assert_eq!(t.total, 100, "total must report the whole transcript");
        assert_eq!(t.items.len(), 20, "only the tail is shipped");
        assert_eq!(t.items[0]["id"], "t80", "tail must be the LAST 20, not the first");
        assert_eq!(t.items[19]["id"], "t99");
        // Prompts are collected before trimming, so recall reaches scrolled-off ones.
        assert_eq!(t.prompts.len(), 10, "prompt history must survive the trim");
        assert_eq!(t.prompts[0], "prompt 0");
        assert_eq!(t.prompts[9], "prompt 90");

        // No limit, 0, or a limit past the end: everything, unchanged.
        for lim in [None, Some(0), Some(100), Some(500)] {
            let t = slice_transcript(items.clone(), lim);
            assert_eq!(t.items.len(), 100, "limit {lim:?} should not trim");
            assert_eq!(t.total, 100);
        }
        // Empty transcript stays empty rather than panicking on the drain range.
        let t = slice_transcript(vec![], Some(60));
        assert_eq!((t.items.len(), t.total, t.prompts.len()), (0, 0, 0));
    }

    #[test]
    fn ocx_never_uses_the_users_own_codex_home() {
        // Regression: ocx injects `openai_base_url = <proxy>` into whatever codex
        // home it resolves, defaulting to ~/.codex. Left unset, Hacksor's proxy
        // hijacked the user's standalone `codex` CLI.
        let docker = ocx_codex_home(true);
        let host = ocx_codex_home(false);
        let user_codex = dirs::home_dir().map(|h| h.join(".codex"));
        for (label, got) in [("docker", &docker), ("host", &host)] {
            assert!(!got.is_empty(), "{label}: empty CODEX_HOME would fall back to ~/.codex");
            let p = std::path::Path::new(got);
            if let Some(u) = &user_codex {
                assert_ne!(p, u.as_path(), "{label}: points at the user's own codex home");
                assert!(!p.starts_with(u), "{label}: sits inside the user's codex home");
            }
            assert!(!got.ends_with("/.codex"), "{label}: resolves to a .codex dir");
        }
        // Docker's must be container-local so the bind-mounted host home is
        // unreachable, and the two modes must not collide.
        assert!(docker.starts_with("/tmp/"), "docker home must be container-local, got {docker}");
        assert_ne!(docker, host);
        // ocx stats CODEX_HOME and aborts with ENOENT if it is missing, so the
        // host path must already exist once resolved (the container path is
        // created by the mkdir in the exec'd command).
        assert!(std::path::Path::new(&host).is_dir(), "host codex home was not created: {host}");
    }

    #[test]
    fn developer_prompt_is_the_persona_plus_runtime_context() {
        let out = build_developer_prompt(
            "PERSONA", "ENVNOTE", "agent", "openrouter/~deepseek/deepseek-v4.1-flash",
            None, "default", None, None,
        );
        assert!(out.starts_with("PERSONA"), "persona stays the cache prefix");
        assert!(out.contains("ENVNOTE"));
        assert!(out.find("ENVNOTE").unwrap() < out.find("<runtime_context>").unwrap());
        // The per-model operating specs were removed: no model gets an overlay.
        assert!(!out.contains("<agent_operating_spec>"));
    }

    #[test]
    fn parse_exec_output_extracts_stdout_and_code() {
        let raw = "Chunk ID: ad27e1\nWall time: 0.0 seconds\nProcess exited with code 0\nOriginal token count: 2\nOutput:\nbishop\n";
        let (out, code) = parse_exec_output(raw);
        assert_eq!(out, "bishop\n");
        assert_eq!(code, Some(0));
    }

    #[test]
    fn the_engagement_tree_is_laid_out_and_never_clobbered() {
        let root = std::env::temp_dir().join(format!("hacksor-ledger-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("target.example.com");
        create_ledger_tree(&dir, "target.example.com").unwrap();
        for sub in ["artifacts", "deliverables", "_quarantine_not_in_delivery", "ledger/entries"] {
            assert!(dir.join(sub).is_dir(), "missing {sub}");
        }
        for f in ["ENGAGEMENT_NOTES.md", "RESIDUAL_STATE.md", "ledger/index.md", "README.md", "findings.jsonl"] {
            assert!(dir.join(f).is_file(), "missing {f}");
        }
        // Resuming must not wipe what the engagement already recorded.
        std::fs::write(dir.join("ENGAGEMENT_NOTES.md"), "REAL WORK").unwrap();
        std::fs::write(dir.join("findings.jsonl"), r#"{"id":"x"}"#).unwrap();
        create_ledger_tree(&dir, "target.example.com").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("ENGAGEMENT_NOTES.md")).unwrap(), "REAL WORK");
        assert!(std::fs::read_to_string(dir.join("findings.jsonl")).unwrap().contains(r#""id""#));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_engagement_read_cannot_escape_its_directory() {
        let root = std::env::temp_dir().join(format!("hacksor-eng-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("artifacts")).unwrap();
        std::fs::write(root.join("artifacts/scan.txt"), "ok").unwrap();
        // A secret OUTSIDE the engagement, which the panel must never reach.
        std::fs::write(root.parent().unwrap().join("outside.txt"), "secret").unwrap();

        assert!(engagement_path(&root, "artifacts/scan.txt").is_some());
        for escape in ["../outside.txt", "artifacts/../../outside.txt", "/etc/hosts"] {
            assert!(
                engagement_path(&root, escape).is_none(),
                "{escape} should not resolve inside the engagement"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn findings_are_ordered_worst_first() {
        let mut v = ["low", "critical", "info", "high", "medium"];
        v.sort_by_key(|s| severity_rank(s));
        assert_eq!(v, ["critical", "high", "medium", "low", "info"]);
        // An unknown severity sorts last rather than first.
        assert!(severity_rank("banana") >= severity_rank("low"));
    }

    #[test]
    fn narrowing_the_mount_keeps_the_codex_home() {
        let tmp = std::env::temp_dir().join(format!("hacksor-mount-{}", std::process::id()));
        let work = tmp.join("engagement");
        let home = tmp.join("codex-home");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        // Off: the whole home, one bind, exactly as before.
        let (h, _c, extra) = runtime_binds(&home, work.to_str().unwrap(), false);
        assert!(extra.is_empty(), "the default must not add binds");
        assert_eq!(h, crate::platform::home_mount().0);

        // On: the working directory, PLUS the codex home — without it the
        // harness cannot read its own config and the agent simply breaks.
        let (h, _c, extra) = runtime_binds(&home, work.to_str().unwrap(), true);
        assert_eq!(h, work.to_str().unwrap());
        assert_eq!(extra.len(), 1, "the codex home must still be mounted");
        assert!(extra[0].starts_with(&home.display().to_string()));

        // On, but the working directory does not exist: fall back rather than
        // start a container the agent cannot work in.
        let (h, _c, extra) = runtime_binds(&home, "/no/such/dir", true);
        assert_eq!(h, crate::platform::home_mount().0);
        assert!(extra.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn context_size_comes_from_the_last_usage_record_not_the_running_total() {
        // `turn_token_usage` accumulates over the whole session and reached
        // millions on a real engagement; `usage` is the single response, and
        // the last one is the current context size.
        let roll = concat!(
            r#"{"type":"token_usage_record","payload":{"usage":{"input_tokens":1000,"output_tokens":10,"total_tokens":1010},"turn_token_usage":{"input_tokens":1000}}}"#, "\n",
            r#"{"type":"response_item","payload":{"text":"noise"}}"#, "\n",
            r#"{"type":"token_usage_record","payload":{"usage":{"input_tokens":188911,"output_tokens":1049,"total_tokens":189960},"turn_token_usage":{"input_tokens":6297861}}}"#, "\n",
        );
        assert_eq!(last_context_tokens(roll), Some(189_960));
        // No records at all: the caller falls back to the character estimate.
        assert_eq!(last_context_tokens(r#"{"type":"response_item"}"#), None);
        // total_tokens missing: derive it from the parts.
        let parts = r#"{"type":"token_usage_record","payload":{"usage":{"input_tokens":700,"output_tokens":42}}}"#;
        assert_eq!(last_context_tokens(parts), Some(742));
    }

    #[test]
    fn a_windows_reserved_device_name_never_becomes_a_directory() {
        // Windows rejects these up to the first dot; renaming only there would
        // put the same engagement in two different directories per platform.
        for raw in ["nul", "CON", "https://nul.example.com", "com1", "lpt9.target.test"] {
            let slug = target_slug(raw);
            let stem = slug.split('.').next().unwrap_or(&slug);
            assert!(
                !["con", "prn", "aux", "nul", "com1", "lpt9"].contains(&stem),
                "{raw} produced the reserved name {slug}"
            );
            assert!(!slug.is_empty());
        }
        // Ordinary targets are untouched.
        assert_eq!(target_slug("conference.example.com"), "conference.example.com");
        assert_eq!(target_slug("https://nul.example.com"), "nul-target.example.com");
        assert_eq!(target_slug("nul"), "nul-target");
    }

    #[test]
    fn only_a_tunnel_on_the_default_route_counts_as_vpn() {
        // macOS keeps utun0-5 up for its own services, so the NAME matters only
        // because we ask it of the default route, never of the interface list.
        for yes in ["utun3", "tun0", "wg0", "ipsec1", "ppp0", "NordLynx",
                    "Mullvad VPN", "ProtonVPN TUN", "tailscale0"] {
            assert!(is_tunnel_iface(yes), "{yes} should read as a tunnel");
        }
        for no in ["en0", "eth0", "wlan0", "bridge100", "lo0", "Ethernet", "Wi-Fi", ""] {
            assert!(!is_tunnel_iface(no), "{no} should NOT read as a tunnel");
        }
    }

    #[test]
    fn a_non_engagement_first_message_names_no_target() {
        // The case that produced a `hey-how-are-u` directory.
        for none in ["none", "None", "none.", "None — this is a greeting", "no target named", ""] {
            assert_eq!(clean_target(none), "", "{none:?}");
        }
        // A sentence is the model ignoring the instruction; better no directory
        // than a bad one.
        assert_eq!(clean_target("The user appears to be asking about a scan of their staging host"), "");
        assert_eq!(clean_target(&"x".repeat(80)), "");
        // Real answers survive, including the decoration models add.
        assert_eq!(clean_target("app.example.com"), "app.example.com");
        assert_eq!(clean_target("\"target\""), "target");
        assert_eq!(clean_target("10.0.0.5"), "10.0.0.5");
        assert_eq!(clean_target("Example Org"), "Example Org");
    }

    #[test]
    fn an_existing_engagement_is_reused_not_duplicated() {
        let root = std::env::temp_dir().join(format!("hacksor-reuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // One real engagement, plus a lookalike directory that is NOT one.
        let real = root.join("target");
        create_ledger_tree(&real, "target").unwrap();
        std::fs::create_dir_all(root.join("target-notes")).unwrap();

        // The same target again, however it is spelled, lands in the same place.
        for again in ["target", "Target", "target.example", "www.target.example", "tar-get"] {
            let slug = target_slug(again);
            let found = find_existing_engagement(&root, &slug);
            assert_eq!(found.as_deref(), Some(real.as_path()), "{again} did not reuse the engagement");
        }
        // An unrelated target still gets its own directory.
        assert!(find_existing_engagement(&root, &target_slug("other-target.example")).is_none());
        // A directory without the notes file is not an engagement.
        assert!(find_existing_engagement(&root, "target-notes").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_target_becomes_a_safe_directory_name() {
        assert_eq!(target_slug("https://app.example.com/login?next=/x"), "app.example.com");
        assert_eq!(target_slug("HTTP://Example.COM"), "example.com");
        assert_eq!(target_slug("10.48.184.199:9001"), "10.48.184.199");
        assert_eq!(target_slug("  Example Org  "), "example-org");
        assert_eq!(target_slug("example.test"), "example.test");
        // Separators never collapse into something that climbs out of the root.
        for nasty in ["../../etc", "..", "/", "../x", "a/../b"] {
            let s = target_slug(nasty);
            assert!(!s.contains(".."), "{nasty} -> {s}");
            assert!(!s.contains('/'), "{nasty} -> {s}");
            assert!(!s.starts_with('.'), "{nasty} -> {s}");
        }
        // Empty or unusable input still names a directory.
        for empty in ["", "   ", "???", "///"] {
            assert!(target_slug(empty).starts_with("session-"), "{empty}");
        }
        // Long targets are bounded.
        assert!(target_slug(&"a".repeat(200)).chars().count() <= 48);
    }

    #[test]
    fn the_engagement_skeleton_names_what_each_file_is_for() {
        let files = ledger_skeleton("target.example.com");
        let names: Vec<&str> = files.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["ENGAGEMENT_NOTES.md", "RESIDUAL_STATE.md", "ledger/index.md", "README.md"]);
        for (name, body) in &files {
            assert!(body.contains("target.example.com"), "{name} does not name the target");
            assert!(!body.is_empty());
        }
        // The notes file carries the two sections that survive a compaction.
        let notes = &files[0].1;
        assert!(notes.contains("## Current state"));
        assert!(notes.contains("## Ruled out"));
    }

    #[test]
    fn a_generated_title_is_reduced_to_a_few_clean_words() {
        // What models actually reply with.
        assert_eq!(clean_title("Nmap Scan Of Staging"), "Nmap Scan Of Staging");
        assert_eq!(clean_title("\"Quoted Title\""), "Quoted Title");
        assert_eq!(clean_title("Title: IDOR On Orders"), "IDOR On Orders");
        assert_eq!(clean_title("**Bold Title**"), "Bold Title");
        assert_eq!(clean_title("  Spaced   Out  Title \n"), "Spaced Out Title");
        assert_eq!(clean_title("Subdomain Enumeration."), "Subdomain Enumeration");
        // Several lines: take the first usable one, not the commentary.
        assert_eq!(clean_title("\nLogin Bypass\n\nThis title describes…"), "Login Bypass");

        // Unusable answers return "" so the caller keeps the truncated prompt
        // rather than putting prose in the sidebar.
        assert_eq!(clean_title(""), "");
        assert_eq!(clean_title("   \n  "), "");
        assert_eq!(
            clean_title("Sure! Here is a concise title that describes what the user is asking about today"),
            ""
        );
        assert_eq!(clean_title(&"x".repeat(80)), "");
    }

    #[test]
    fn version_compare_only_claims_a_real_upgrade() {
        assert!(version_newer("1.7.0", "1.6.0"));
        assert!(version_newer("v1.7.0", "1.6.0"));   // tags carry a leading v
        assert!(version_newer("1.6.1", "1.6.0"));
        assert!(version_newer("2.0.0", "1.99.99"));
        assert!(version_newer("1.10.0", "1.9.0"));   // not a string compare
        assert!(!version_newer("1.6.0", "1.6.0"));
        assert!(!version_newer("1.5.3", "1.6.0"));   // never downgrade
        assert!(!version_newer("1.6.0-beta.1", "1.6.0")); // pre-release of the same
        // An unreadable feed must never look like an update.
        for junk in ["", "latest", "nightly", "v", "x.y.z"] {
            assert!(!version_newer(junk, "1.6.0"), "{junk}");
        }
    }

    #[test]
    fn changelog_section_is_cut_at_the_next_release() {
        // Verbatim shape of what semantic-release writes: `#` for a minor,
        // `##` for a patch.
        let log = "\
# [1.6.0](https://x/compare/v1.5.3...v1.6.0) (2026-10-18)\n\
\n\
\n\
### Bug Fixes\n\
\n\
* **ui:** highlight restored messages up front ([d043172](https://x))\n\
\n\
\n\
### Features\n\
\n\
* **branch:** branch a chat at one message ([124733b](https://x))\n\
\n\
## [1.5.3](https://x/compare/v1.5.2...v1.5.3) (2026-10-13)\n\
\n\
### Bug Fixes\n\
\n\
* **models:** list ocx's direct upstreams ([5b72446](https://x))\n";
        let s = changelog_section(log, "1.6.0");
        assert!(s.starts_with("# [1.6.0]"));
        assert!(s.contains("**branch:**"));
        // The previous release must NOT bleed in. (Its version string DOES
        // appear in this heading's own compare URL, so check for its heading
        // and its entries rather than the bare number.)
        assert!(!s.contains("## [1.5.3]"), "{s}");
        assert!(!s.contains("list ocx's direct upstreams"));

        // A patch section works the same way, and is bounded below.
        let p = changelog_section(log, "1.5.3");
        assert!(p.starts_with("## [1.5.3]"));
        assert!(p.contains("list ocx's direct upstreams"));
        assert!(!p.contains("# [1.6.0]"));
        assert!(!p.contains("**branch:**"));

        // A version with no section (a dev build between releases) is empty, so
        // the first-run modal stays quiet instead of showing the wrong notes.
        assert_eq!(changelog_section(log, "9.9.9"), "");
        assert_eq!(changelog_section("", "1.6.0"), "");
    }

    #[test]
    fn the_shipped_changelog_has_a_section_for_this_build() {
        // Guards the release wiring: the bundle jobs check out the tag
        // semantic-release made, so a shipped binary must contain its own
        // notes. If this fails, "what's new" would come up empty for users.
        let v = app_version();
        let s = changelog_section(CHANGELOG, &v);
        assert!(!s.is_empty(), "CHANGELOG.md has no section for {v}");
        assert!(s.contains(&v), "the {v} section does not name its version");
    }

    #[test]
    fn only_a_real_opencodex_cmdline_may_be_killed() {
        // The two command lines this machine actually serves the proxy with
        // (from ~/.opencodex/serving-runtimes.json): bundled in the container,
        // and installed on the host.
        for real in [
            "/usr/local/lib/node_modules/@bitkyc08/opencodex/node_modules/bun/bin/bun.exe /usr/local/lib/node_modules/@bitkyc08/opencodex/src/cli/index.ts start --port 10100",
            "/opt/homebrew/lib/node_modules/@bitkyc08/opencodex/node_modules/bun/bin/bun.exe /opt/homebrew/lib/node_modules/@bitkyc08/opencodex/src/cli/index.ts",
            "/usr/local/bin/ocx start",
        ] {
            assert!(cmdline_is_opencodex(real.to_string()), "{real}");
        }
        // The pid file is shared with the container, so it can name a pid that
        // on the host is something else entirely. None of these may be killed.
        for other in [
            "/usr/libexec/opendirectoryd",
            "/Applications/Hacksor.app/Contents/MacOS/hacksor",
            "/sbin/launchd",
            "node /usr/local/bin/codex app-server",
        ] {
            assert!(!cmdline_is_opencodex(other.to_string()), "{other}");
        }
    }

    #[test]
    fn ocx_account_row_is_matched_on_the_provider_column() {
        // Verbatim shape of `ocx account current anthropic` (2.88.0).
        let listing = "PROVIDER   TYPE   ID                                PLAN/LABEL       PRIORITY  STATUS\n\
                       anthropic  oauth  2f038da95488712dead06cf1004be7ce  l***8@gmail.com  -         active\n";
        let (row, active) = parse_account_row(listing, "anthropic").unwrap();
        assert!(row.starts_with("anthropic  oauth"));
        assert!(active);

        // The header must never be mistaken for a row, and a different
        // provider's row must not answer for this one.
        assert!(parse_account_row(listing, "anthropic2").is_none());
        assert!(parse_account_row(listing, "openai").is_none());
        assert!(parse_account_row("", "anthropic").is_none());

        // A paused account is stored but not usable.
        let paused = "anthropic  oauth  abc  l***8@gmail.com  -  paused\n";
        let (_, active) = parse_account_row(paused, "anthropic").unwrap();
        assert!(!active);
    }

    #[test]
    fn provider_names_are_validated_before_reaching_a_shell() {
        assert_eq!(safe_provider_name(" Anthropic ").as_deref(), Some("anthropic"));
        assert_eq!(safe_provider_name("github-copilot").as_deref(), Some("github-copilot"));
        assert_eq!(safe_provider_name("anthropic2").as_deref(), Some("anthropic2"));
        for bad in ["", "   ", "anthropic; rm -rf /", "an thropic", "a/b", "$(id)", "../etc", &"x".repeat(41)] {
            assert!(safe_provider_name(bad).is_none(), "{bad} must be rejected");
        }
    }

    #[test]
    fn branch_cut_keeps_only_the_branch_point_and_what_came_before() {
        let items = parse_rollout(
            &[
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer one"}]}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"second"}]}}"#,
                r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c1","arguments":"{\"cmd\":\"id\"}"}}"#,
                r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"Chunk ID: x\nProcess exited with code 0\nOutput:\nroot\n"}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer two"}]}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"third"}]}}"#,
            ]
            .join("\n"),
        );
        // Branching from the SECOND prompt keeps it and drops its answer.
        let at_prompt = truncate_at_user(&items, 2, false);
        let types: Vec<&str> = at_prompt.iter().map(|i| i["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["userMessage", "agentMessage", "userMessage"]);
        assert_eq!(at_prompt.last().unwrap()["content"][0]["text"], "second");

        // Branching from that prompt's ANSWER keeps the whole turn (command
        // included) and still drops the next prompt.
        let at_answer = truncate_at_user(&items, 2, true);
        let types: Vec<&str> = at_answer.iter().map(|i| i["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            ["userMessage", "agentMessage", "userMessage", "commandExecution", "agentMessage"]
        );
        assert_eq!(at_answer.last().unwrap()["text"], "answer two");

        // Out of range (a live message not yet on disk) keeps everything, and
        // ordinal 0 is not a branch point at all.
        assert_eq!(truncate_at_user(&items, 9, true).len(), items.len());
        assert!(truncate_at_user(&items, 0, true).is_empty());
    }

    #[test]
    fn compactions_are_counted_only_up_to_the_branch_point() {
        let user = |t: &str| format!(r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{t}"}}]}}}}"#);
        let text = [
            user("one"),
            r#"{"type":"compacted","payload":{"message":"summary"}}"#.to_string(),
            user("two"),
            user("<environment_context>\n cwd </environment_context>"),
            r#"{"type":"compacted","payload":{"message":"summary"}}"#.to_string(),
            user("three"),
        ]
        .join("\n");
        // Branching at the first prompt is before either compaction.
        assert_eq!(compactions_before(&text, 1, false), 0);
        // Keeping the first answer runs up to the second prompt, past the first
        // compaction.
        assert_eq!(compactions_before(&text, 1, true), 1);
        assert_eq!(compactions_before(&text, 2, false), 1);
        // The injected <environment_context> wrapper is not a prompt, so the
        // third prompt is the one after the second compaction.
        assert_eq!(compactions_before(&text, 3, false), 2);
        // The same enumeration the cut uses: three prompts, not four.
        let items = parse_rollout(&text);
        assert_eq!(items.iter().filter(|i| i["type"] == "userMessage").count(), 3);
    }

    #[test]
    fn branch_markdown_carries_messages_commands_and_a_compaction_notice() {
        let items = parse_rollout(
            &[
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"scan the host"}]}}"#,
                r#"{"type":"response_item","payload":{"type":"reasoning","content":[{"type":"reasoning_text","text":"internal"}]}}"#,
                r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c1","arguments":"{\"cmd\":\"nmap -p- 10.0.0.1\"}"}}"#,
                r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"Chunk ID: x\nProcess exited with code 0\nOutput:\n22/tcp open ssh\n"}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"port 22 is open"}]}}"#,
            ]
            .join("\n"),
        );
        let md = branch_markdown(&items, 2);
        assert!(md.contains("## User\n\nscan the host"));
        assert!(md.contains("## Assistant\n\nport 22 is open"));
        assert!(md.contains("$ nmap -p- 10.0.0.1"));
        assert!(md.contains("22/tcp open ssh"));
        assert!(md.contains("compacted 2 time(s)"));
        // Reasoning is deliberately left out.
        assert!(!md.contains("internal"));
        // With no compaction there is no notice to explain.
        assert!(!branch_markdown(&items, 0).contains("compacted"));
    }

    #[test]
    fn the_engagement_a_thread_worked_in_is_picked_by_mentions() {
        let root = std::env::temp_dir().join(format!("hacksor-eng-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for name in ["alpha.example.com", "beta.example.com", "notes-only"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        // Only the first two are engagements.
        for name in ["alpha.example.com", "beta.example.com"] {
            std::fs::write(root.join(name).join("ENGAGEMENT_NOTES.md"), "x").unwrap();
        }

        // The thread ran in beta and only glanced at alpha.
        let text = "cd beta.example.com; nmap beta.example.com; curl beta.example.com; see alpha.example.com";
        assert_eq!(
            engagement_worked_in(&root, text).unwrap().file_name().unwrap(),
            std::ffi::OsStr::new("beta.example.com")
        );

        // A tie is not a guess.
        assert!(engagement_worked_in(&root, "alpha.example.com beta.example.com").is_none());
        // Neither is silence.
        assert!(engagement_worked_in(&root, "nothing relevant here").is_none());
        // A plain directory with no notes file is not an engagement.
        assert!(engagement_worked_in(&root, "notes-only notes-only notes-only").is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn engagement_dir_recovered_from_preamble_or_cwd() {
        // 1. The preamble names it outright.
        let roll = r#"{"payload":{"content":"<engagement_ledger>\nThis session's engagement directory is /Users/op/sec/target.example.com. It is the record of this\nengagement"}}"#;
        assert_eq!(
            engagement_dir_in_rollout(roll).as_deref(),
            Some("/Users/op/sec/target.example.com")
        );

        // A thread that moved on recovers the engagement it is actually on.
        let two = format!("{roll}\n{}", roll.replace("target.example.com", "second.example.com"));
        assert_eq!(
            engagement_dir_in_rollout(&two).as_deref(),
            Some("/Users/op/sec/second.example.com")
        );

        // 2. No preamble — fall back to the cwd on the session-meta line. This
        //    is the case that actually occurs: the preamble rides on the second
        //    message and a thread can end before sending one.
        let meta = "{\"payload\":{\"cwd\":\"/Users/op/sec/third.example.com\"}}\n{\"payload\":{\"content\":\"hi\"}}";
        assert_eq!(
            engagement_dir_in_rollout(meta).as_deref(),
            Some("/Users/op/sec/third.example.com")
        );

        // Nothing to go on.
        assert_eq!(engagement_dir_in_rollout(r#"{"payload":{"content":"hello"}}"#), None);
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
