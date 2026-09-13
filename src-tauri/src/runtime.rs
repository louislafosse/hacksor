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

use bollard::container::{Config, CreateContainerOptions, RemoveContainerOptions, StartContainerOptions};
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::image::{BuildImageOptions, CreateImageOptions, TagImageOptions};
use bollard::models::{DeviceMapping, HostConfig, PortBinding};
use bollard::Docker;
use futures_util::StreamExt;

/// Connect to the local Docker engine (socket/pipe per platform).
pub fn connect() -> Result<Docker, String> {
    Docker::connect_with_local_defaults().map_err(|e| format!("cannot reach the Docker engine: {e}"))
}

/// Is the Docker engine reachable? (Replaces the old `docker --version` probe.)
pub async fn docker_present() -> bool {
    match Docker::connect_with_local_defaults() {
        Ok(d) => d.ping().await.is_ok(),
        Err(_) => false,
    }
}

/// Does a local image with this name exist?
pub async fn image_exists(name: &str) -> bool {
    match connect() {
        Ok(d) => d.inspect_image(name).await.is_ok(),
        Err(_) => false,
    }
}

/// Is a container with this name currently running?
pub async fn container_running(name: &str) -> bool {
    let Ok(d) = connect() else { return false };
    match d.inspect_container(name, None).await {
        Ok(info) => info.state.and_then(|s| s.running).unwrap_or(false),
        Err(_) => false,
    }
}

/// Force-remove a container (ignore "not found").
pub async fn remove_container(name: &str) {
    if let Ok(d) = connect() {
        let _ = d
            .remove_container(name, Some(RemoveContainerOptions { force: true, ..Default::default() }))
            .await;
    }
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

/// Create + start the runtime container with the platform-appropriate config.
/// Mirrors the previous `docker run` argv exactly, expressed via the API.
pub async fn create_and_start(spec: &RunSpec) -> Result<(), String> {
    let d = connect()?;
    remove_container(&spec.name).await;

    let mut host_config = HostConfig {
        binds: Some(vec![format!("{}:{}", spec.host_home, spec.container_home)]),
        // Run a real init (Docker's bundled tini) as PID 1 so it reaps zombies.
        // Our CMD is `sleep infinity`, which never reaps children — a single stale
        // `<defunct> mitmdump` (or any spawned tool) would otherwise accumulate and,
        // with a pgrep-based liveness check, wedge restarts. tini fixes that.
        init: Some(true),
        ..Default::default()
    };
    if spec.linux {
        host_config.network_mode = Some("host".into());
        host_config.extra_hosts = Some(vec!["host.docker.internal:host-gateway".into()]);
        host_config.cap_add = Some(vec!["SYS_ADMIN".into(), "NET_ADMIN".into(), "NET_RAW".into()]);
        host_config.security_opt = Some(vec!["seccomp=unconfined".into(), "apparmor=unconfined".into()]);
        if spec.tun {
            host_config.devices = Some(vec![DeviceMapping {
                path_on_host: Some("/dev/net/tun".into()),
                path_in_container: Some("/dev/net/tun".into()),
                cgroup_permissions: Some("rwm".into()),
            }]);
        }
    } else {
        // Docker Desktop VM: no host networking. Keep the caps Desktop allows and
        // publish the requested ports to the host loopback.
        host_config.cap_add = Some(vec!["NET_ADMIN".into(), "NET_RAW".into()]);
        host_config.security_opt = Some(vec!["seccomp=unconfined".into()]);
        let mut pb = std::collections::HashMap::new();
        for p in &spec.publish_ports {
            pb.insert(
                format!("{p}/tcp"),
                Some(vec![PortBinding {
                    host_ip: Some("127.0.0.1".into()),
                    host_port: Some(p.to_string()),
                }]),
            );
        }
        if !pb.is_empty() {
            host_config.port_bindings = Some(pb);
        }
    }

    let config = Config {
        image: Some(spec.image.clone()),
        cmd: Some(vec!["sleep".into(), "infinity".into()]),
        env: Some(vec![format!("HOME={}", spec.container_home)]),
        working_dir: Some(spec.container_home.clone()),
        host_config: Some(host_config),
        ..Default::default()
    };

    d.create_container(Some(CreateContainerOptions { name: spec.name.clone(), platform: None }), config)
        .await
        .map_err(|e| format!("create container: {e}"))?;
    d.start_container(&spec.name, None::<StartContainerOptions<String>>)
        .await
        .map_err(|e| format!("start container: {e}"))?;
    Ok(())
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
    let d = connect()?;
    let exec = d
        .create_exec(
            container,
            CreateExecOptions {
                cmd: Some(cmd),
                user,
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                attach_stdin: stdin.map(|_| true),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| e.to_string())?
        .id;
    let out = match d.start_exec(&exec, None).await.map_err(|e| e.to_string())? {
        StartExecResults::Attached { mut output, mut input } => {
            if let Some(data) = stdin {
                use tokio::io::AsyncWriteExt;
                let _ = input.write_all(data.as_bytes()).await;
                let _ = input.shutdown().await;
            }
            let mut out = String::new();
            while let Some(chunk) = output.next().await {
                if let Ok(msg) = chunk {
                    out.push_str(&msg.to_string());
                }
            }
            out
        }
        StartExecResults::Detached => String::new(),
    };
    let code = d.inspect_exec(&exec).await.ok().and_then(|r| r.exit_code).unwrap_or(-1);
    Ok((out, code))
}

/// Start a command in the container detached (fire-and-forget), as `user`.
pub async fn exec_detached(container: &str, cmd: Vec<String>, user: Option<String>) -> Result<(), String> {
    let d = connect()?;
    let exec = d
        .create_exec(
            container,
            CreateExecOptions::<String> {
                cmd: Some(cmd),
                user,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| e.to_string())?
        .id;
    // detach:true makes start_exec return immediately without attaching streams.
    let _ = d
        .start_exec(&exec, Some(StartExecOptions { detach: true, ..Default::default() }))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Tag an image locally (e.g. the pulled registry image → `hacksor-runtime:latest`).
pub async fn tag(source: &str, dest_repo: &str, dest_tag: &str) -> Result<(), String> {
    let d = connect()?;
    d.tag_image(source, Some(TagImageOptions { repo: dest_repo, tag: dest_tag }))
        .await
        .map_err(|e| format!("tag image: {e}"))
}

/// Pull an image, invoking `on_progress(current_bytes, total_bytes)` as layers
/// download so the caller can render a progress bar.
pub async fn pull<F: Fn(i64, i64)>(image: &str, on_progress: F) -> Result<(), String> {
    let d = connect()?;
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
    let d = connect()?;
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
