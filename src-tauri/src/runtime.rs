//! All Docker operations go through the Docker Engine API via `bollard` — a
//! single, multiplatform path (Unix socket on Linux/macOS, named pipe on
//! Windows) with no dependency on the `docker` CLI being on PATH. This module is
//! the one place that talks to Docker for image/container lifecycle, exec, pull
//! and build.
//!
//! (The persistent app-server connection in `harness.rs` still uses `docker exec
//! -i` for its long-lived bidirectional stdio stream; the CLI it uses is itself
//! multiplatform. Everything else — provisioning, one-shot execs, inspects,
//! pulls, builds, teardown — is bollard.)

use std::path::Path;
use std::process::Stdio;

use bollard::image::{BuildImageOptions, CreateImageOptions, TagImageOptions};
use bollard::Docker;
use futures_util::StreamExt;

/// Connect to the local Docker engine (socket/pipe per platform) and negotiate
/// the API version with the daemon. The negotiation matters on Docker Desktop
/// (especially Windows): without it bollard uses its built-in default API
/// version, and a mismatch makes POST endpoints like `/containers/create`
/// return a body bollard can't parse ("expected value at line 1 column 1")
/// even though GET calls (ping/inspect/build) succeed. If negotiation itself
/// fails we fall back to the default client so nothing regresses.
pub async fn connect() -> Result<Docker, String> {
    let d = Docker::connect_with_local_defaults()
        .map_err(|e| format!("cannot reach the Docker engine: {e}"))?;
    match d.negotiate_version().await {
        Ok(d) => Ok(d),
        Err(_) => Docker::connect_with_local_defaults()
            .map_err(|e| format!("cannot reach the Docker engine: {e}")),
    }
}

/// Is the Docker engine reachable? (Replaces the old `docker --version` probe.)
pub async fn docker_present() -> bool {
    match Docker::connect_with_local_defaults() {
        Ok(d) => d.ping().await.is_ok(),
        Err(_) => false,
    }
}

/// Is the `docker` CLI present on PATH? Distinct from `docker_present`, which
/// checks whether the daemon is actually reachable: Docker Desktop puts
/// `docker` on PATH as soon as it's installed, even before the app/VM has
/// been started, so this alone tells "not installed" apart from "installed
/// but the daemon isn't running yet".
pub fn docker_installed() -> bool {
    crate::platform::which("docker").is_some()
}

/// Human-readable reason the Docker runtime can't be used right now, or
/// `None` if it's ready. Never claims Docker is "not installed" when the CLI
/// is present but the daemon just hasn't been started.
pub async fn docker_unavailable_reason() -> Option<String> {
    if !docker_installed() {
        return Some(
            "Docker is not installed. Install Docker Desktop (docker.com/products/docker-desktop), or switch Runtime to Host in Settings.".into(),
        );
    }
    if !docker_present().await {
        return Some(
            "Docker is installed, but the Docker daemon isn't running. Start Docker Desktop, or switch Runtime to Host in Settings.".into(),
        );
    }
    None
}

/// Does a local image with this name exist?
pub async fn image_exists(name: &str) -> bool {
    match connect().await {
        Ok(d) => d.inspect_image(name).await.is_ok(),
        Err(_) => false,
    }
}

/// Is a container with this name currently running?
pub async fn container_running(name: &str) -> bool {
    let Ok(d) = connect().await else { return false };
    match d.inspect_container(name, None).await {
        Ok(info) => info.state.and_then(|s| s.running).unwrap_or(false),
        Err(_) => false,
    }
}

/// Force-remove a container (ignore "not found"). Via the `docker` CLI so it
/// works on Docker Desktop for Windows like the other container ops.
pub async fn remove_container(name: &str) {
    let _ = docker_cli(&["rm", "-f", name], None).await;
}

/// How the runtime container attaches to the network + host, per platform.
pub struct RunSpec {
    pub image: String,
    pub name: String,
    pub host_home: String,      // bind mount source (host path)
    pub container_home: String, // bind mount target + HOME + workdir
    pub linux: bool,            // Linux => --network host + full caps + tun
    pub tun: bool,              // /dev/net/tun present (Linux)
    pub publish_ports: Vec<u16>,// ports to publish to 127.0.0.1 (Docker Desktop)
}

/// Create + start the runtime container via the `docker` CLI. bollard's
/// `create_container` POST fails on Docker Desktop for Windows ("expected value
/// at line 1 column 1") even though build/inspect (GET) succeed — a named-pipe
/// transport quirk for that endpoint. The CLI drives the pipe reliably and is
/// already a dependency (the harness runs `docker exec`). This builds the same
/// container the API config described.
pub async fn create_and_start(spec: &RunSpec) -> Result<(), String> {
    // Clear any stale container so `--name` never conflicts.
    let _ = docker_cli(&["rm", "-f", &spec.name], None).await;

    let binds = format!("{}:{}", spec.host_home, spec.container_home);
    let home_env = format!("HOME={}", spec.container_home);
    // `--init` runs Docker's bundled tini as PID 1 so it reaps zombies (our CMD
    // `sleep infinity` never would, which otherwise wedges pgrep-based restarts).
    let mut args: Vec<String> =
        vec!["run".into(), "-d".into(), "--name".into(), spec.name.clone(), "--init".into()];
    if spec.linux {
        args.push("--network".into());
        args.push("host".into());
        args.push("--add-host".into());
        args.push("host.docker.internal:host-gateway".into());
        for c in ["SYS_ADMIN", "NET_ADMIN", "NET_RAW"] {
            args.push("--cap-add".into());
            args.push(c.into());
        }
        args.push("--security-opt".into());
        args.push("seccomp=unconfined".into());
        args.push("--security-opt".into());
        args.push("apparmor=unconfined".into());
        if spec.tun {
            args.push("--device".into());
            args.push("/dev/net/tun:/dev/net/tun".into());
        }
    } else {
        // Docker Desktop VM: no host networking; keep the allowed caps and publish
        // the requested ports to the host loopback.
        for c in ["NET_ADMIN", "NET_RAW"] {
            args.push("--cap-add".into());
            args.push(c.into());
        }
        args.push("--security-opt".into());
        args.push("seccomp=unconfined".into());
        for p in &spec.publish_ports {
            args.push("-p".into());
            args.push(format!("127.0.0.1:{p}:{p}/tcp"));
        }
    }
    args.push("-v".into());
    args.push(binds);
    args.push("-e".into());
    args.push(home_env);
    args.push("-w".into());
    args.push(spec.container_home.clone());
    args.push(spec.image.clone());
    args.push("sleep".into());
    args.push("infinity".into());

    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let (out, code) = docker_cli(&argv, None).await?;
    if code != 0 {
        return Err(format!("create container: docker run failed: {}", out.trim()));
    }
    Ok(())
}

/// Run the `docker` CLI with args, capturing combined stdout+stderr and the exit
/// code. This is the reliable cross-platform path against Docker Desktop (the
/// Windows named pipe in particular); `docker` must be on PATH — the harness
/// relies on it too. On Windows a hidden console avoids a flashing window.
async fn docker_cli(args: &[&str], stdin: Option<&str>) -> Result<(String, i64), String> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.args(args)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to run docker (is Docker Desktop running and on PATH?): {e}"))?;
    if let Some(data) = stdin {
        if let Some(mut si) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = si.write_all(data.as_bytes()).await;
            let _ = si.shutdown().await;
        }
    }
    let out = child.wait_with_output().await.map_err(|e| e.to_string())?;
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(&err);
    }
    Ok((s, out.status.code().map(|c| c as i64).unwrap_or(-1)))
}

/// Run a command in the container and return `(combined stdout+stderr, exit
/// code)`. `user` is `Some("uid:gid")` (Linux) or `None` (root). `stdin` is piped
/// when present. Exit code is `-1` if it couldn't be determined.
pub async fn exec_output(
    container: &str,
    cmd: Vec<String>,
    user: Option<String>,
    stdin: Option<&str>,
) -> Result<(String, i64), String> {
    // Via the `docker` CLI (see docker_cli): bollard's create_exec POST hits the
    // same Docker Desktop / Windows named-pipe failure as create_container.
    let mut args: Vec<String> = vec!["exec".into()];
    if stdin.is_some() {
        args.push("-i".into());
    }
    if let Some(u) = &user {
        args.push("-u".into());
        args.push(u.clone());
    }
    args.push(container.to_string());
    args.extend(cmd);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    docker_cli(&argv, stdin).await
}

/// Start a command in the container detached (fire-and-forget), as `user`.
pub async fn exec_detached(container: &str, cmd: Vec<String>, user: Option<String>) -> Result<(), String> {
    let mut args: Vec<String> = vec!["exec".into(), "-d".into()];
    if let Some(u) = &user {
        args.push("-u".into());
        args.push(u.clone());
    }
    args.push(container.to_string());
    args.extend(cmd);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let (out, code) = docker_cli(&argv, None).await?;
    if code != 0 {
        return Err(format!("docker exec -d failed: {}", out.trim()));
    }
    Ok(())
}

/// Tag an image locally (e.g. the pulled registry image → `hacksor-runtime:latest`).
pub async fn tag(source: &str, dest_repo: &str, dest_tag: &str) -> Result<(), String> {
    let d = connect().await?;
    d.tag_image(source, Some(TagImageOptions { repo: dest_repo, tag: dest_tag }))
        .await
        .map_err(|e| format!("tag image: {e}"))
}

/// Pull an image, invoking `on_progress(current_bytes, total_bytes)` as layers
/// download so the caller can render a progress bar.
pub async fn pull<F: Fn(i64, i64)>(image: &str, on_progress: F) -> Result<(), String> {
    let d = connect().await?;
    let opts = CreateImageOptions { from_image: image, ..Default::default() };
    let mut stream = d.create_image(Some(opts), None, None);
    let mut layers: std::collections::HashMap<String, (i64, i64)> = std::collections::HashMap::new();
    while let Some(item) = stream.next().await {
        let info = item.map_err(|e| e.to_string())?;
        if let Some(id) = info.id.clone() {
            if let Some(pd) = info.progress_detail {
                let tot = pd.total.unwrap_or(0);
                let cur = pd.current.unwrap_or(0);
                if tot > 0 {
                    layers.insert(id, (cur, tot));
                }
            }
        }
        let (cur, tot) = layers.values().fold((0i64, 0i64), |(a, b), (c, t)| (a + c, b + t));
        on_progress(cur, tot);
    }
    Ok(())
}

/// Build an image from a Dockerfile (as text) via the Engine API. The build
/// context is a minimal in-memory tar containing just the Dockerfile. Invokes
/// `on_line(&str)` for each build log line so the caller can surface progress.
pub async fn build<F: Fn(&str)>(dockerfile: &str, tag_name: &str, on_line: F) -> Result<(), String> {
    let d = connect().await?;
    // Tar the context (just the Dockerfile).
    let mut tar_buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        let bytes = dockerfile.as_bytes();
        let mut header = tar::Header::new_gnu();
        header.set_path("Dockerfile").map_err(|e| e.to_string())?;
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, bytes).map_err(|e| e.to_string())?;
        builder.finish().map_err(|e| e.to_string())?;
    }
    let opts = BuildImageOptions {
        dockerfile: "Dockerfile".to_string(),
        t: tag_name.to_string(),
        rm: true,
        ..Default::default()
    };
    let mut stream = d.build_image(opts, None, Some(tar_buf.into()));
    let mut last_err: Option<String> = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(info) => {
                if let Some(s) = info.stream {
                    let t = s.trim();
                    if !t.is_empty() {
                        on_line(t);
                    }
                }
                if let Some(err) = info.error {
                    last_err = Some(err);
                }
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    match last_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Whether `/dev/net/tun` exists (Linux VPN tun device passthrough).
pub fn tun_present() -> bool {
    Path::new("/dev/net/tun").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Live integration check against the real Docker engine. Ignored by default
    // (needs Docker + the runtime image); run with:
    //   cargo test -p hacksor runtime::tests::create_exec_remove -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn create_exec_remove() {
        assert!(docker_present().await, "docker engine not reachable");
        let home = crate::platform::home_dir().to_string_lossy().to_string();
        let spec = RunSpec {
            image: "hacksor-runtime:latest".into(),
            name: "hacksor-runtime-test".into(),
            host_home: home.clone(),
            container_home: home.clone(),
            linux: cfg!(target_os = "linux"),
            tun: tun_present(),
            publish_ports: vec![],
        };
        create_and_start(&spec).await.expect("create+start");
        assert!(container_running(&spec.name).await, "container should be running");
        let (out, code) = exec_output(&spec.name, vec!["sh".into(), "-c".into(), "echo hacksor-ok".into()], None, None)
            .await
            .expect("exec");
        assert!(out.contains("hacksor-ok"), "exec output was: {out:?}");
        assert_eq!(code, 0, "echo should exit 0");
        remove_container(&spec.name).await;
        assert!(!container_running(&spec.name).await, "container should be gone");
    }

    // Validates the in-memory tar build context against the real engine.
    #[tokio::test]
    #[ignore]
    async fn build_trivial_image() {
        assert!(docker_present().await, "docker engine not reachable");
        let df = "FROM alpine:latest\nRUN echo hacksor-build-ok\n";
        let lines = std::sync::atomic::AtomicUsize::new(0);
        build(df, "hacksor-build-test:latest", |_l| { lines.fetch_add(1, std::sync::atomic::Ordering::SeqCst); }).await.expect("build");
        assert!(image_exists("hacksor-build-test:latest").await, "built image should exist");
        assert!(lines.load(std::sync::atomic::Ordering::SeqCst) > 0, "should have streamed build log lines");
    }
}
