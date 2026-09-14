//! Codex app-server client.
//!
//! Hacksor drives the agent loop by spawning the locally installed `codex
//! app-server` and speaking newline-delimited JSON-RPC to it over stdio. There
//! is no cloud infrastructure: the harness runs on this machine and its shell /
//! file / patch tools execute here. Server notifications are forwarded verbatim
//! to the Tauri frontend as `hacksor://event` payloads (the UI already renders
//! the app-server notification shapes).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex};

use crate::models::Provider;

#[derive(Debug, Clone, Copy)]
pub enum Permission {
    Full,
    Ask,
    ReadOnly,
}

impl Permission {
    pub fn parse(s: &str) -> Self {
        match s {
            // auto_review behaves like ask at the codex layer (approvals are
            // requested); the Hacksor frontend auto-decides them via a reviewer.
            "ask_approval" | "ask" | "auto_review" => Permission::Ask,
            "read_only" | "read-only" => Permission::ReadOnly,
            _ => Permission::Full,
        }
    }

    /// app-server `approvalPolicy` value.
    fn approval_policy(self) -> &'static str {
        match self {
            Permission::Full => "never",
            Permission::Ask => "unlessTrusted",
            Permission::ReadOnly => "unlessTrusted",
        }
    }

    /// app-server `sandbox` value (kebab-case in the app-server protocol).
    fn sandbox(self) -> &'static str {
        match self {
            // Full access runs directly on the host with no sandbox helper.
            Permission::Full => "danger-full-access",
            Permission::Ask => "danger-full-access",
            Permission::ReadOnly => "read-only",
        }
    }
}

/// Where the codex app-server runs: directly on the host, or inside the bundled
/// Docker runtime container (so users only need Docker).
#[derive(Clone)]
pub enum LaunchMode {
    Host,
    /// `user` is "uid:gid" so the app-server runs as the HOST user inside the
    /// container — codex-home is bind-mounted, and running as root would create
    /// root-owned files there that the host app (running as the user) can't read.
    /// `user` is `Some("uid:gid")` on Linux so the app-server runs as the HOST
    /// user inside the container (codex-home is bind-mounted, and root would
    /// create root-owned files the host user can't read). `None` on macOS /
    /// Windows, where Docker Desktop maps bind-mount ownership to the user, so
    /// the container's default (root) is fine.
    Docker { container: String, user: Option<String> },
}

pub struct StartParams {
    pub provider: Provider,
    pub model_slug: String,
    pub cwd: PathBuf,
    pub permission: Permission,
    pub developer_instructions: String,
}

/// Per-turn overrides passed on `turn/start`.
#[derive(Default)]
pub struct TurnOverrides {
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub effort: Option<String>,
    pub developer_instructions: Option<String>,
}

/// A live app-server connection shared across all chats in the session.
pub struct Harness {
    app: AppHandle,
    stdin: Mutex<ChildStdin>,
    next_id: AtomicI64,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>,
    /// Latest turn id per thread, for interrupts.
    turns: Arc<Mutex<HashMap<String, String>>>,
    /// Pending approval request ids keyed by an opaque token handed to the UI.
    approvals: Arc<Mutex<HashMap<String, Value>>>,
    /// False once the app-server process has exited (stdout hit EOF). A dead
    /// harness is dropped and respawned by `ensure_harness`.
    alive: Arc<AtomicBool>,
    /// Set when we deliberately tear the harness down (settings/key/runtime
    /// change, app exit). The reader loop then skips the "stopped unexpectedly"
    /// error so an intentional restart never looks like a crash.
    intentional: Arc<AtomicBool>,
    /// True if this harness runs the app-server inside the Docker container, so
    /// `ensure_harness` can respawn when the runtime setting changes.
    pub is_docker: bool,
    _child: Mutex<Child>,
}

/// The last few non-empty lines of the app-server's captured stderr, for error
/// messages. Empty when nothing was captured.
async fn stderr_tail(buf: &Arc<Mutex<String>>) -> String {
    let s = buf.lock().await;
    let mut tail: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).rev().take(4).collect();
    tail.reverse();
    tail.join("\n")
}

impl Harness {
    pub async fn new(codex_home: PathBuf, app: AppHandle, launch: LaunchMode) -> Result<Arc<Self>> {
        // Register the Playwright MCP browser server only in Docker mode, where it
        // is pre-installed (fast). On host, `npx @playwright/mcp` would fetch on
        // first run and could stall the first message — the persona uses the
        // agent-browser / Playwright-script fallback there instead.
        // In Docker mode the container is the isolation boundary, so codex must
        // NOT also run its bubblewrap sandbox — bwrap can't create namespaces
        // inside an unprivileged container ("No permissions to create a new
        // namespace") and blocks every command. Also enables the Playwright MCP
        // browser server there (pre-installed).
        let is_docker = matches!(launch, LaunchMode::Docker { .. });
        write_provider_config(&codex_home, is_docker).context("write codex config.toml")?;

        let mut cmd = match &launch {
            LaunchMode::Host => {
                let bin = codex_binary();
                let mut c = tokio::process::Command::new(&bin);
                c.arg("app-server").env("CODEX_HOME", &codex_home);
                c
            }
            LaunchMode::Docker { container, user } => {
                // Run the app-server inside the container. On Linux use the host
                // user (-u uid:gid) so files it writes to the bind-mounted
                // codex-home are user-owned; on macOS/Windows run as root
                // (ownership is mapped by Docker Desktop). codex resolves each
                // provider's `env_key` from its own environment, so forward any
                // provider keys the host exported into the `env` prefix.
                let mut args: Vec<String> = vec!["exec".into(), "-i".into()];
                if let Some(u) = user {
                    args.push("-u".into());
                    args.push(u.clone());
                }
                args.push(container.clone());
                args.push("env".into());
                // CODEX_HOME is the container-side path (identity on Linux/macOS;
                // remapped under the Windows home mount).
                args.push(format!("CODEX_HOME={}", crate::platform::container_codex_home(&codex_home)));
                for provider in crate::models::Provider::ALL {
                    let key = provider.env_key();
                    if let Ok(val) = std::env::var(key) {
                        if !val.is_empty() {
                            args.push(format!("{key}={val}"));
                        }
                    }
                }
                args.push("codex".into());
                args.push("app-server".into());
                let mut c = tokio::process::Command::new("docker");
                c.args(&args);
                c
            }
        };
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Capture stderr so an unexpected exit can report WHY (was discarded).
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // On Windows the app-server is a long-lived `docker exec`; hide its console.
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let mut child = cmd
            .spawn()
            .context("spawn codex app-server (host: is codex installed? docker: is the runtime container running?)")?;

        let stdin = child.stdin.take().context("codex stdin")?;
        let stdout = child.stdout.take().context("codex stdout")?;
        let stderr = child.stderr.take().context("codex stderr")?;

        // Accumulate the app-server's stderr (bounded) for crash diagnostics.
        let stderr_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        {
            let buf = Arc::clone(&stderr_buf);
            tauri::async_runtime::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut b = buf.lock().await;
                    b.push_str(&line);
                    b.push('\n');
                    if b.len() > 4000 {
                        let cut = b.len() - 4000;
                        *b = b.split_off(cut);
                    }
                }
            });
        }

        let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let turns: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
        let approvals: Arc<Mutex<HashMap<String, Value>>> = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let intentional = Arc::new(AtomicBool::new(false));

        let harness = Arc::new(Self {
            app: app.clone(),
            stdin: Mutex::new(stdin),
            next_id: AtomicI64::new(1),
            pending: Arc::clone(&pending),
            turns: Arc::clone(&turns),
            approvals: Arc::clone(&approvals),
            alive: Arc::clone(&alive),
            intentional: Arc::clone(&intentional),
            is_docker,
            _child: Mutex::new(child),
        });

        // Reader loop. When stdout hits EOF the app-server has exited: mark the
        // harness dead, fail every in-flight request, and tell the UI so it can
        // surface the drop; the next command respawns a fresh harness.
        {
            let harness = Arc::clone(&harness);
            let stderr_buf = Arc::clone(&stderr_buf);
            tauri::async_runtime::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Ok(value) = serde_json::from_str::<Value>(&line) {
                        harness.handle_incoming(value).await;
                    }
                }
                harness.alive.store(false, Ordering::SeqCst);
                let tail = stderr_tail(&stderr_buf).await;
                let exit_msg = if tail.is_empty() { "codex app-server exited".to_string() } else { format!("codex app-server exited: {tail}") };
                let mut pend = harness.pending.lock().await;
                for (_, tx) in pend.drain() {
                    let _ = tx.send(Err(exit_msg.clone()));
                }
                // Only surface a crash notice for an UNEXPECTED exit. A deliberate
                // teardown (settings/key/runtime change, app exit) set `intentional`,
                // so the next command silently respawns a fresh harness instead.
                if !harness.intentional.load(Ordering::SeqCst) {
                    let base = "The agent backend stopped unexpectedly and was restarted. Please resend your last message.";
                    let message = if tail.is_empty() { base.to_string() } else { format!("{base}\n\n{tail}") };
                    let _ = harness.app.emit(
                        "hacksor://event",
                        json!({ "method": "error", "params": { "message": message } }),
                    );
                }
            });
        }

        // Surface the app-server's own stderr if the initialize handshake fails
        // because the process died on startup.
        if let Err(e) = harness.initialize().await {
            let tail = stderr_tail(&stderr_buf).await;
            return Err(if tail.is_empty() { e } else { anyhow::anyhow!("{e}\n{tail}") });
        }
        Ok(harness)
    }

    /// True while the underlying app-server process is running.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    async fn initialize(&self) -> Result<()> {
        self.request(
            "initialize",
            json!({
                "clientInfo": { "name": "hacksor", "title": "Hacksor", "version": "0.1.0" },
                "capabilities": { "experimentalApi": true }
            }),
        )
        .await
        .map_err(|e| anyhow!("initialize failed: {e}"))?;
        self.notify("initialized", json!({})).await?;
        Ok(())
    }

    /// Send a request and await its response.
    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let msg = json!({ "id": id, "method": method, "params": params });
        if let Err(e) = self.write(&msg).await {
            self.pending.lock().await.remove(&id);
            return Err(e.to_string());
        }
        match rx.await {
            Ok(res) => res,
            Err(_) => Err("codex connection closed".into()),
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write(&json!({ "method": method, "params": params })).await
    }

    async fn write(&self, msg: &Value) -> Result<()> {
        let mut line = serde_json::to_string(msg).map_err(|e| anyhow!(e))?;
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Route one incoming JSON-RPC message: response, notification, or server
    /// request (approvals and the like).
    async fn handle_incoming(&self, value: Value) {
        let has_method = value.get("method").is_some();
        let id = value.get("id").cloned();

        // Response to one of our requests: has id + (result|error), no method.
        if !has_method {
            if let Some(id) = id.as_ref().and_then(|v| v.as_i64()) {
                if let Some(tx) = self.pending.lock().await.remove(&id) {
                    let res = if let Some(err) = value.get("error") {
                        Err(err
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("codex error")
                            .to_string())
                    } else {
                        Ok(value.get("result").cloned().unwrap_or(Value::Null))
                    };
                    let _ = tx.send(res);
                }
            }
            return;
        }

        let method = value.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = value.get("params").cloned().unwrap_or(Value::Null);

        // Server -> client request: has both method and id. These are approvals
        // and similar; they need a response or the turn stalls.
        if id.is_some() {
            self.handle_server_request(method, id.unwrap(), params).await;
            return;
        }

        // Plain notification: track turn ids, then forward to the UI.
        if method == "turn/started" {
            if let (Some(t), Some(turn)) = (
                params.get("threadId").and_then(|v| v.as_str()),
                params
                    .get("turnId")
                    .or_else(|| params.get("turn").and_then(|t| t.get("id")))
                    .and_then(|v| v.as_str()),
            ) {
                self.turns.lock().await.insert(t.to_string(), turn.to_string());
            }
        }
        let _ = self.app.emit("hacksor://event", json!({ "method": method, "params": params }));
    }

    async fn handle_server_request(&self, method: &str, id: Value, params: Value) {
        match method {
            "item/commandExecution/requestApproval" | "execCommandApproval" => {
                let token = self.stash_approval(id).await;
                let _ = self.app.emit(
                    "hacksor://event",
                    json!({ "method": "approval/exec", "params": with_token(params, &token) }),
                );
            }
            "item/fileChange/requestApproval" | "applyPatchApproval" => {
                let token = self.stash_approval(id).await;
                let _ = self.app.emit(
                    "hacksor://event",
                    json!({ "method": "approval/patch", "params": with_token(params, &token) }),
                );
            }
            // Best-effort default for other server requests so turns don't hang.
            _ => {
                let _ = self.write(&json!({ "id": id, "result": {} })).await;
            }
        }
    }

    async fn stash_approval(&self, id: Value) -> String {
        let token = format!("apr-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        self.approvals.lock().await.insert(token.clone(), id);
        token
    }

    pub async fn start_thread(&self, params: StartParams) -> Result<String> {
        // Non-ephemeral so history persists locally, which enables branch (fork)
        // and try-again (rollback).
        let mut p = json!({
            "model": params.model_slug,
            "modelProvider": params.provider.codex_provider_id(),
            "cwd": params.cwd.to_string_lossy(),
            "approvalPolicy": params.permission.approval_policy(),
            "sandbox": params.permission.sandbox(),
        });
        if !params.developer_instructions.is_empty() {
            p["developerInstructions"] = json!(params.developer_instructions);
        }
        let res = self.request("thread/start", p).await.map_err(|e| anyhow!(e))?;
        let thread_id = res
            .get("thread")
            .and_then(|t| t.get("id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("thread/start returned no thread id: {res}"))?
            .to_string();
        Ok(thread_id)
    }

    pub async fn send_message(
        &self,
        thread_id: &str,
        text: String,
        turn: TurnOverrides,
        images: Vec<String>,
    ) -> Result<()> {
        let mut input = vec![json!({ "type": "text", "text": text })];
        for path in images.into_iter().filter(|p| !p.is_empty()) {
            input.push(json!({ "type": "localImage", "path": path }));
        }
        let mut params = json!({
            "threadId": thread_id,
            "input": input,
        });
        // Per-turn overrides. Codex applies them to this turn onward on the SAME
        // thread, so switching model/provider/cwd preserves the conversation.
        if let Some(dir) = turn.cwd.filter(|d| !d.is_empty()) {
            params["cwd"] = json!(dir);
        }
        if let Some(model) = turn.model.filter(|m| !m.is_empty()) {
            params["model"] = json!(model);
        }
        if let Some(provider) = turn.provider.filter(|p| !p.is_empty()) {
            params["modelProvider"] = json!(provider);
        }
        if let Some(effort) = turn.effort.filter(|e| !e.is_empty()) {
            params["effort"] = json!(effort);
        }
        // Re-assert the HackerAI persona each turn so it is always present, even
        // after a model/provider switch or a resume.
        if let Some(dev) = turn.developer_instructions.filter(|d| !d.is_empty()) {
            params["developerInstructions"] = json!(dev);
        }
        // In Docker mode the container is the sandbox, so force full access every
        // turn. This also un-poisons OLD threads that were created before the
        // config default and resume with sandbox:None (network blocked).
        if self.is_docker {
            params["sandbox"] = json!("danger-full-access");
            params["approvalPolicy"] = json!("never");
        }
        // A fresh app-server (after a harness restart from a runtime/key change or
        // a crash-respawn) hasn't loaded this thread into memory, so turn/start
        // fails with "thread not found". Resume it once, then retry — otherwise
        // the message silently does nothing.
        match self.request("turn/start", params.clone()).await {
            Ok(_) => Ok(()),
            Err(e) if e.to_lowercase().contains("thread not found") => {
                let mut rp = json!({ "threadId": thread_id });
                if self.is_docker {
                    rp["sandbox"] = json!("danger-full-access");
                    rp["approvalPolicy"] = json!("never");
                }
                let _ = self.request("thread/resume", rp).await;
                self.request("turn/start", params).await.map_err(|e| anyhow!(e))?;
                Ok(())
            }
            Err(e) => Err(anyhow!(e)),
        }
    }

    /// List stored threads (most recent first) for the Recents section. A high
    /// limit so the sidebar's "Show more" can page through the full history.
    pub async fn list_threads(&self) -> Result<Value> {
        let res = self
            .request("thread/list", json!({ "limit": 500 }))
            .await
            .map_err(|e| anyhow!(e))?;
        Ok(res.get("data").cloned().unwrap_or(Value::Array(vec![])))
    }

    /// Resume a stored thread and return its reconstructed history. In Docker
    /// mode the container is the sandbox, so resume the thread with full access —
    /// without this, resume restores sandbox:None and every command (network
    /// especially) is blocked by codex's restrictive default.
    pub async fn resume_thread(&self, thread_id: &str) -> Result<Value> {
        let mut params = json!({ "threadId": thread_id });
        if self.is_docker {
            params["sandbox"] = json!("danger-full-access");
            params["approvalPolicy"] = json!("never");
        }
        let res = self
            .request("thread/resume", params)
            .await
            .map_err(|e| anyhow!(e))?;
        Ok(res.get("thread").cloned().unwrap_or(Value::Null))
    }

    /// Branch: fork the thread into a new thread id with copied history.
    pub async fn fork_thread(&self, thread_id: &str) -> Result<String> {
        let res = self
            .request("thread/fork", json!({ "threadId": thread_id }))
            .await
            .map_err(|e| anyhow!(e))?;
        res.get("thread")
            .and_then(|t| t.get("id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("thread/fork returned no thread id"))
    }

    /// Try again: drop the last turn, then re-submit the given input.
    pub async fn regenerate(
        &self,
        thread_id: &str,
        text: String,
        turn: TurnOverrides,
        images: Vec<String>,
        num_turns: i64,
    ) -> Result<()> {
        // Ensure the thread is loaded on this app-server first (a restart drops
        // in-memory threads), otherwise rollback silently no-ops on "not found".
        let _ = self
            .request("thread/resume", json!({ "threadId": thread_id }))
            .await;
        // Best-effort rollback of the last `num_turns` turns (1 for a plain retry,
        // more for edit-and-resend of an earlier message); if there is no history
        // yet the resubmission simply appends a new turn.
        let _ = self
            .request("thread/rollback", json!({ "threadId": thread_id, "numTurns": num_turns }))
            .await;
        self.send_message(thread_id, text, turn, images).await
    }

    pub async fn interrupt(&self, thread_id: &str) -> Result<()> {
        let turn_id = self.turns.lock().await.get(thread_id).cloned();
        let mut params = json!({ "threadId": thread_id });
        if let Some(turn) = turn_id {
            params["turnId"] = json!(turn);
        }
        // Ignore errors: the turn may already be finished.
        let _ = self.request("turn/interrupt", params).await;
        Ok(())
    }

    /// Resolve an approval request identified by the opaque `token`.
    pub async fn resolve_approval(&self, token: &str, approve: bool) -> Result<()> {
        let id = self
            .approvals
            .lock()
            .await
            .remove(token)
            .ok_or_else(|| anyhow!("unknown approval {token}"))?;
        let decision = if approve { "accept" } else { "decline" };
        self.write(&json!({ "id": id, "result": { "decision": decision } }))
            .await
    }

    /// Kill the app-server process. The reader loop ends when stdout closes.
    pub async fn shutdown(&self) {
        // Mark the exit as deliberate so the reader loop doesn't raise a
        // "stopped unexpectedly" crash notice when the process goes away.
        self.intentional.store(true, Ordering::SeqCst);
        let mut child = self._child.lock().await;
        let _ = child.start_kill();
    }
}

fn with_token(mut params: Value, token: &str) -> Value {
    if let Some(obj) = params.as_object_mut() {
        obj.insert("token".to_string(), json!(token));
    }
    params
}

/// Locate the `codex` binary: `HACKSOR_CODEX_BIN`, then PATH, then `~/.local/bin`.
fn codex_binary() -> String {
    if let Ok(p) = std::env::var("HACKSOR_CODEX_BIN") {
        if !p.is_empty() {
            return p;
        }
    }
    if let Some(home) = dirs::home_dir() {
        let local = home.join(".local/bin/codex");
        if local.exists() {
            return local.to_string_lossy().to_string();
        }
    }
    "codex".to_string()
}

/// Write every supported provider block into `codex_home/config.toml`. All
/// providers use the OpenAI-compatible `responses` wire API.
fn write_provider_config(codex_home: &PathBuf, is_docker: bool) -> Result<()> {
    std::fs::create_dir_all(codex_home)?;
    let mut toml = String::from(
        "# Managed by Hacksor. Do not edit the provider blocks by hand.\n\
# Every provider routes through the local OpenCodex proxy; the upstream is\n\
# chosen by the model-id prefix (openrouter/…, vercel-ai-gateway/…, anthropic/…).\n\
model_provider = \"opencodex\"\n\
# Do not load a target directory's AGENTS.md/CLAUDE.md: it pollutes the Hacksor\n\
# persona and is a prompt-injection vector when auditing untrusted repos.\n\
project_doc_max_bytes = 0\n\
# codex 0.152+ ships update_plan disabled by default (it must be opted back in\n\
# per-session); without this the model has no plan/todo tool and any attempt\n\
# to call update_plan fails with \"unsupported call\". The persona explicitly\n\
# uses plan tracking for multi-step engagements, so turn it back on.\n\
[tools.update_plan]\n\
enabled = true\n",
    );
    if is_docker {
        // The Docker container is the isolation boundary; disable codex's own
        // bubblewrap sandbox (it can't create namespaces inside an unprivileged
        // container and blocks every command).
        toml.push_str("sandbox_mode = \"danger-full-access\"\n");
    }
    let browser_mcp = is_docker;
    // In Docker mode the OpenCodex proxy runs INSIDE the same container as the
    // app-server, so it's reached at the container's own `127.0.0.1` on every
    // platform. In host mode both are on the host — also `127.0.0.1`. So the
    // base_url is always the pinned loopback port.
    for provider in Provider::ALL {
        if provider.is_local_proxy() {
            // OpenCodex proxy: no env_key (local, optional bearer via header).
            toml.push_str(&format!(
                "\n[model_providers.{id}]\nname = \"{name}\"\nbase_url = \"{url}\"\nwire_api = \"responses\"\nrequest_max_retries = 4\nstream_max_retries = 5\n[model_providers.{id}.env_http_headers]\nx-opencodex-api-key = \"{env}\"\n",
                id = provider.id(),
                name = provider.display(),
                url = provider.base_url(),
                env = provider.env_key(),
            ));
        } else {
            toml.push_str(&format!(
                "\n[model_providers.{}]\nname = \"{}\"\nbase_url = \"{}\"\nenv_key = \"{}\"\nwire_api = \"responses\"\nrequest_max_retries = 4\nstream_max_retries = 5\n",
                provider.id(),
                provider.display(),
                provider.base_url(),
                provider.env_key(),
            ));
        }
    }
    // Camoufox MCP (redf0x1/camofox-mcp) gives the agent first-class browser
    // tools (snapshot/click/type_text/camofox_hover/camofox_press_key/sessions/…)
    // driven by Camoufox — a STEALTH Firefox that resists Cloudflare/Turnstile
    // and HUMANIZES clicks + typing, so logins/registrations look human. It talks
    // to the `camofox-browser` server on 127.0.0.1:9377 (started in the
    // container). Fallbacks (agent-browser, Playwright/Chromium) remain per the
    // persona. Use the image's INSTALLED camofox-mcp (not `npx @latest`) so it
    // stays in lockstep with the patched camofox-browser and doesn't refetch on
    // every start; generous startup timeout for the first-run engine warm-up.
    if browser_mcp {
        toml.push_str(
            "\n[mcp_servers.camofox]\ncommand = \"camofox-mcp\"\nargs = []\nstartup_timeout_ms = 180000\ntool_timeout_sec = 300\n[mcp_servers.camofox.env]\nCAMOFOX_URL = \"http://127.0.0.1:9377\"\n",
        );
    }
    std::fs::write(codex_home.join("config.toml"), toml)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_toml_has_all_providers_and_hardening() {
        let dir = std::env::temp_dir().join(format!("hacksor-cfg-{}", std::process::id()));
        write_provider_config(&dir, true).unwrap();
        let toml = std::fs::read_to_string(dir.join("config.toml")).unwrap();

        // Anti prompt-injection: never load a target repo's AGENTS.md/CLAUDE.md.
        assert!(toml.contains("project_doc_max_bytes = 0"));
        // Everything routes through the OpenCodex proxy; all three blocks present.
        assert!(toml.contains("model_provider = \"opencodex\""));
        assert!(toml.contains("[model_providers.openrouter]"));
        assert!(toml.contains("[model_providers.vercel]"));
        assert!(toml.contains("[model_providers.opencodex]"));
        // Codex only speaks the Responses wire API.
        assert!(toml.contains("wire_api = \"responses\""));
        assert!(!toml.contains("wire_api = \"chat\""));
        // Every block routes through the local OpenCodex proxy — no direct
        // OpenRouter/Vercel endpoints, and all reached via the proxy header.
        assert!(!toml.contains("openrouter.ai"));
        assert!(!toml.contains("ai-gateway.vercel.sh"));
        // All three blocks point at the in-container/host loopback proxy port.
        assert_eq!(toml.matches("127.0.0.1:10100/v1").count(), 3);
        assert_eq!(toml.matches("x-opencodex-api-key").count(), 3);
        assert!(!toml.contains("env_key"));
        // Playwright MCP browser server is registered.
        assert!(toml.contains("[mcp_servers.playwright]"));
        assert!(toml.contains("@playwright/mcp"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
