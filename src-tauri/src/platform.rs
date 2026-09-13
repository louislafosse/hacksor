//! Cross-platform primitives. Everything that used to shell out to `sh -c` plus
//! Unix utilities (`id`, `kill`, `pgrep`) lives here, branched per OS, so Hacksor
//! runs on Linux, macOS and Windows.
//!
//! Design notes for the Docker runtime across platforms:
//! - **Linux**: Docker runs on the real host. The container uses `--network host`
//!   (its `127.0.0.1` == the host loopback), raw-socket caps and `/dev/net/tun`,
//!   and runs as the host uid:gid so bind-mounted files aren't root-owned.
//! - **macOS / Windows**: Docker Desktop runs in a VM. No `--network host`; the
//!   container reaches host services (the OpenCodex proxy) via
//!   `host.docker.internal`. Bind-mount ownership is mapped to the user by Docker
//!   Desktop, so the container runs as root (no uid:gid dance). On Windows the
//!   host home path is not a valid Linux path, so codex-home and the working dir
//!   are mounted at fixed container paths and host paths are translated.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const IS_LINUX: bool = cfg!(target_os = "linux");
#[allow(dead_code)] // part of the platform API; referenced on non-Linux builds / by readers
pub const IS_MACOS: bool = cfg!(target_os = "macos");
pub const IS_WINDOWS: bool = cfg!(target_os = "windows");

/// On Windows the host home path (`C:\Users\…`) isn't a valid Linux path, so the
/// whole user home is bind-mounted at this single fixed container path and every
/// host path under it is translated. On Linux/macOS the home is mounted at its
/// real path (identity), so no translation is needed.
pub const WIN_HOME: &str = "/hacksorhome";

pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Cross-platform PATH lookup (`;`-separated on Windows, tries executable
/// extensions there; `:`-separated elsewhere).
pub fn which(tool: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    let exts: &[&str] = if IS_WINDOWS { &["", ".exe", ".cmd", ".bat"] } else { &[""] };
    for dir in std::env::split_paths(&path) {
        for ext in exts {
            let cand = dir.join(format!("{tool}{ext}"));
            if cand.is_file() {
                return Some(cand.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// The `-u uid:gid` identity for `docker exec`, or `None` to run as the container
/// default (root). Only meaningful on Linux, where a root process in the
/// container would create root-owned files in the bind-mounted home. Docker
/// Desktop (macOS/Windows) maps bind-mount ownership to the user, so we don't.
pub fn docker_exec_user() -> Option<String> {
    if !IS_LINUX {
        return None;
    }
    let uid = id_flag("-u")?;
    let gid = id_flag("-g")?;
    Some(format!("{uid}:{gid}"))
}

#[cfg(unix)]
fn id_flag(flag: &str) -> Option<String> {
    let o = Command::new("id").arg(flag).output().ok()?;
    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}
#[cfg(not(unix))]
fn id_flag(_flag: &str) -> Option<String> {
    None
}

/// Kill a process by PID (host side). `kill` on Unix, `taskkill /F` on Windows.
pub fn kill_pid(pid: i64) {
    let mut cmd = if IS_WINDOWS {
        let mut c = Command::new("taskkill");
        c.args(["/PID", &pid.to_string(), "/F"]);
        c
    } else {
        let mut c = Command::new("kill");
        c.arg(pid.to_string());
        c
    };
    let _ = cmd.stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// Translate a host path to the path the container sees. Identity on Linux/macOS
/// (home mounted at its real path); on Windows, any path under the user home is
/// remapped under the single `WIN_HOME` mount.
pub fn to_container_path(p: &Path) -> String {
    if !IS_WINDOWS {
        return p.to_string_lossy().into_owned();
    }
    match p.strip_prefix(home_dir()) {
        Ok(rel) => join_unix(WIN_HOME, rel),
        Err(_) => WIN_HOME.to_string(), // outside home — best effort: home root
    }
}

/// Container path for the bind-mounted codex-home.
pub fn container_codex_home(codex_home: &Path) -> String {
    to_container_path(codex_home)
}

/// Container path for a working directory.
pub fn container_working_dir(working_dir: &Path) -> String {
    to_container_path(working_dir)
}

/// The `HOME` env value inside the container.
pub fn container_home_env() -> String {
    if IS_WINDOWS {
        WIN_HOME.to_string()
    } else {
        home_dir().to_string_lossy().into_owned()
    }
}

/// The `-v host:container` bind mount for the user home, and the `HOME` value.
pub fn home_mount() -> (String, String) {
    let host = home_dir().to_string_lossy().into_owned();
    let container = container_home_env();
    (format!("{host}:{container}"), container)
}

fn join_unix(base: &str, rel: &Path) -> String {
    let r = rel.to_string_lossy().replace('\\', "/");
    if r.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{r}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn container_paths_are_identity_off_windows() {
        let ch = Path::new("/home/u/.local/share/app/codex-home");
        let wd = Path::new("/home/u/proj");
        if !IS_WINDOWS {
            assert_eq!(container_codex_home(ch), "/home/u/.local/share/app/codex-home");
            assert_eq!(container_working_dir(wd), "/home/u/proj");
            assert_eq!(to_container_path(&ch.join("proxy/flows.jsonl")), "/home/u/.local/share/app/codex-home/proxy/flows.jsonl");
            assert_eq!(container_home_env(), home_dir().to_string_lossy());
        }
    }

    #[test]
    fn join_unix_builds_forward_slash_paths() {
        assert_eq!(join_unix("/hacksor", Path::new("proxy/addon.py")), "/hacksor/proxy/addon.py");
        assert_eq!(join_unix("/work", Path::new("")), "/work");
    }
}
