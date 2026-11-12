//! Docker on `docker.sock` (the Engine API, HTTP/1.1 over the Unix
//! socket). Compose resolves the current service environment before a swap.
//! The new container gets the remaining
//! settings of the old one from its inspect record, so the binds, ports,
//! networks, labels (the compose project too), and the seccomp profile stay
//! the same. Recovery uses the original environment, not the edited deployment.
//!
//! The env, the command, the entrypoint, the labels, and the user of the
//! old *image* are not copied: Docker merges the ones of the new image at
//! create. So a new image that changes its own `ENV` or `CMD` takes
//! effect. The compose labels (`com.docker.compose.*`) are the exception:
//! they always stay. An image that `docker compose build` made carries the
//! project and service labels too, and the release image does not, so
//! without them the next update cannot find the container (TER-55).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request, StatusCode, header};
use hyper_util::rt::TokioIo;
use serde_json::{Map, Value, json};

use crate::engine::{BoxFut, Container, Engine, ExecOut, ExecState, Health, Res};
use crate::protocol::Image;

/// The default Docker socket.
pub const DEFAULT_SOCKET: &str = "/var/run/docker.sock";
/// The compose label of the project and of the service.
pub const LABEL_PROJECT: &str = "com.docker.compose.project";
pub const LABEL_SERVICE: &str = "com.docker.compose.service";
/// The compose label of the image ID of a container.
pub const LABEL_COMPOSE_IMAGE: &str = "com.docker.compose.image";
/// The service of the daemon in compose.yaml.
pub const SERVICE: &str = "terrarium";
/// The OCI label of the version.
pub const LABEL_VERSION: &str = "org.opencontainers.image.version";
/// The stop timeout when the container sets none (compose.yaml: 650 s).
pub const DEFAULT_STOP_TIMEOUT: u64 = 650;

/// The Docker engine.
#[derive(Debug, Clone)]
pub struct Docker {
    pub socket: PathBuf,
    /// The name of the terrarium container. `None`: the service
    /// `terrarium` of the compose project of the updater's own container
    /// (`HOSTNAME`).
    pub target: Option<String>,
}

fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn short(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

impl Docker {
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Res<(StatusCode, Bytes)> {
        // Pulls and online backups may be long, but no transport operation
        // may wait forever. Hot-swap recovery has a tighter outer deadline.
        self.call_until(
            method,
            path,
            body,
            tokio::time::Instant::now() + Duration::from_secs(900),
        )
        .await
    }

    async fn call_until(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        deadline: tokio::time::Instant,
    ) -> Res<(StatusCode, Bytes)> {
        tokio::time::timeout_at(deadline, async {
            let stream = tokio::net::UnixStream::connect(&self.socket)
                .await
                .map_err(|e| format!("cannot reach Docker at {}: {e}", self.socket.display()))?;
            let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
                .await
                .map_err(|e| format!("Docker handshake: {e}"))?;
            struct Connection(tokio::task::JoinHandle<()>);
            impl Drop for Connection {
                fn drop(&mut self) {
                    self.0.abort();
                }
            }
            let _connection = Connection(tokio::spawn(async move {
                let _ = conn.await;
            }));
            let mut b = Request::builder()
                .method(method.clone())
                .uri(path)
                .header(header::HOST, "docker");
            let bytes = match body {
                Some(v) => {
                    b = b.header(header::CONTENT_TYPE, "application/json");
                    Bytes::from(serde_json::to_vec(&v).map_err(|e| e.to_string())?)
                }
                None => Bytes::new(),
            };
            let req = b.body(Full::new(bytes)).map_err(|e| e.to_string())?;
            let resp = sender
                .send_request(req)
                .await
                .map_err(|e| format!("Docker {method} {path}: {e}"))?;
            let status = resp.status();
            let body = resp
                .into_body()
                .collect()
                .await
                .map_err(|e| format!("Docker {method} {path}: {e}"))?
                .to_bytes();
            Ok((status, body))
        })
        .await
        .map_err(|_| "Docker request deadline exceeded (values hidden)".to_owned())?
    }

    /// A call that must succeed; the JSON answer (or `null`).
    async fn ok(&self, method: Method, path: &str, body: Option<Value>) -> Res<Value> {
        let (status, bytes) = self.call(method.clone(), path, body).await?;
        if !status.is_success() && status != StatusCode::NOT_MODIFIED {
            return Err(format!(
                "Docker {method} {path}: {status}: {}",
                message(&bytes)
            ));
        }
        Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn compose_config(&self, container: &Value, reference: &str) -> Res<(Value, String)> {
        let me = std::env::var("HOSTNAME")
            .map_err(|_| "cannot inspect updater deployment mounts (values hidden)".to_owned())?;
        let own = self.inspect(&me).await?;
        let mounts = &own["Mounts"];
        for required in ["/compose", "/deployment"] {
            if !mounts
                .as_array()
                .into_iter()
                .flatten()
                .any(|m| m["Destination"] == required && m["RW"] == false)
            {
                return Err("read-only deployment mounts are missing; recreate the updater with the current compose.yaml; differing keys unavailable (values hidden)".into());
            }
        }
        let labels = &container["Config"]["Labels"];
        let work = labels["com.docker.compose.project.working_dir"]
            .as_str()
            .ok_or_else(|| "missing Compose working directory (values hidden)".to_owned())?;
        let directory = mounted_path(work, mounts)?;
        let files = labels["com.docker.compose.project.config_files"]
            .as_str()
            .ok_or_else(|| "missing Compose configuration paths (values hidden)".to_owned())?;
        let project = labels[LABEL_PROJECT]
            .as_str()
            .ok_or_else(|| "missing Compose project (values hidden)".to_owned())?;
        let service = labels[LABEL_SERVICE]
            .as_str()
            .ok_or_else(|| "missing Compose service (values hidden)".to_owned())?;
        let mut cmd = compose_command();
        cmd.args(["compose", "--project-name", project, "--project-directory"])
            .arg(&directory)
            .current_dir(&directory);
        for file in files.split(',') {
            cmd.arg("--file").arg(mounted_path(file, mounts)?);
        }
        // Compose records explicit --env-file paths. With no explicit
        // override, the project .env is the source (source checkouts).
        if let Some(files) = labels["com.docker.compose.project.environment_file"]
            .as_str()
            .filter(|s| !s.is_empty())
        {
            for file in files.split(',') {
                cmd.arg("--env-file").arg(mounted_path(file, mounts)?);
            }
        } else {
            cmd.arg("--env-file").arg(directory.join(".env"));
        }
        // The deploy wrapper supplies these independently of .env.
        if let Some(root) = mounts
            .as_array()
            .into_iter()
            .flatten()
            .find(|m| m["Destination"] == "/deployment")
            .and_then(|m| m["Source"].as_str())
        {
            cmd.env("TERRARIUM_DATA_DIR", root);
        }
        cmd.env("TERRARIUM_IMAGE", reference);
        cmd.args(["config", "--format", "json"]);
        let output = tokio::time::timeout(Duration::from_secs(60), cmd.kill_on_drop(true).output())
            .await
            .map_err(|_| "Compose environment resolution timed out (values hidden)".to_owned())?
            .map_err(|_| "cannot run Compose environment resolution (values hidden)".to_owned())?;
        // Compose diagnostics and parse errors can contain secrets. Do not
        // include stdout, stderr, or serde's error in logs or run history.
        if !output.status.success() {
            return Err("Compose environment resolution failed; update stopped before swap; differing keys unavailable (values hidden)".into());
        }
        let config: Value = serde_json::from_slice(&output.stdout)
            .map_err(|_| "invalid Compose configuration (values hidden)".to_owned())?;
        Ok((config, service.to_owned()))
    }

    async fn inspect(&self, id: &str) -> Res<Value> {
        self.ok(Method::GET, &format!("/containers/{}/json", enc(id)), None)
            .await
    }

    async fn inspect_image(&self, reference: &str) -> Res<Option<Value>> {
        let (status, bytes) = self
            .call(
                Method::GET,
                &format!("/images/{}/json", enc(reference)),
                None,
            )
            .await?;
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(format!(
                "Docker image {reference}: {status}: {}",
                message(&bytes)
            ));
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| e.to_string())
    }

    /// The ID of the terrarium container.
    async fn target_id(&self) -> Res<String> {
        if let Some(name) = &self.target {
            return Ok(name.clone());
        }
        let me = std::env::var("HOSTNAME").map_err(|_| "HOSTNAME is not set".to_owned())?;
        let own = self.inspect(&me).await.map_err(|e| {
            format!("cannot find the updater's own container ({e}); set TERRARIUM_UPDATER_TARGET")
        })?;
        let project = own["Config"]["Labels"][LABEL_PROJECT]
            .as_str()
            .ok_or_else(|| {
                "the updater runs outside a compose project; set TERRARIUM_UPDATER_TARGET"
                    .to_owned()
            })?
            .to_owned();
        let filters = json!({"label": [
            format!("{LABEL_PROJECT}={project}"),
            format!("{LABEL_SERVICE}={SERVICE}"),
        ]});
        let list = self
            .ok(
                Method::GET,
                &format!(
                    "/containers/json?all=1&filters={}",
                    enc(&filters.to_string())
                ),
                None,
            )
            .await?;
        list.as_array()
            .and_then(|a| a.first())
            .and_then(|c| c["Id"].as_str())
            .map(str::to_owned)
            .ok_or_else(|| format!("no `{SERVICE}` container in the compose project `{project}`"))
    }

    async fn container(&self, id: &str) -> Res<Container> {
        let c = self.inspect(id).await?;
        let image_id = c["Image"].as_str().unwrap_or_default().to_owned();
        let reference = c["Config"]["Image"].as_str().unwrap_or_default().to_owned();
        let img = self.inspect_image(&image_id).await?.unwrap_or(Value::Null);
        let mut image = image_of(&img, &reference);
        // Docker's image field stays unchanged after a binary-only upgrade.
        // Read the selected release identity, not the original container image.
        if img["Config"]["Labels"]["io.terrarium.hot-swap-layout"] == "1" {
            let attempted = self.exec(id, crate::machine::hot_argv("active", &[])).await;
            let active = match attempted {
                Ok(out) if out.code == 0 => out,
                _ => {
                    // A stopped, paused or broken target must still expose its
                    // release identity for status/recovery. Read only metadata
                    // through a disposable container sharing the state mount.
                    let spec = json!({"container": c, "image": img["Config"]});
                    self.run_body(once_body(
                        &spec,
                        &image_id,
                        &crate::machine::hot_argv("active", &[]),
                    ))
                    .await?
                }
            };
            if active.code != 0 {
                return Err(format!(
                    "cannot read active release identity: {}",
                    active.reason()
                ));
            }
            if let Some(selected) = serde_json::from_str::<Option<Image>>(&active.stdout)
                .map_err(|e| format!("invalid active release identity: {e}"))?
            {
                image = selected;
            }
        }
        Ok(Container {
            id: c["Id"].as_str().unwrap_or_default().to_owned(),
            name: c["Name"]
                .as_str()
                .unwrap_or_default()
                .trim_start_matches('/')
                .to_owned(),
            image,
            spec: json!({"container": c, "image": img["Config"]}),
        })
    }

    /// Run `argv` in a new container made from `body`, wait, read its
    /// output, and remove it.
    async fn run_body(&self, body: Value) -> Res<ExecOut> {
        let made = self
            .ok(Method::POST, "/containers/create", Some(body))
            .await?;
        let id = made["Id"].as_str().unwrap_or_default().to_owned();
        let r = async {
            self.ok(Method::POST, &format!("/containers/{id}/start"), None)
                .await?;
            let w = self
                .ok(Method::POST, &format!("/containers/{id}/wait"), None)
                .await?;
            let (_, logs) = self
                .call(
                    Method::GET,
                    &format!("/containers/{id}/logs?stdout=1&stderr=1"),
                    None,
                )
                .await?;
            let (stdout, stderr) = demux(&logs);
            Ok(ExecOut {
                code: w["StatusCode"].as_i64().unwrap_or(-1),
                stdout,
                stderr,
            })
        }
        .await;
        let _ = self
            .call(Method::DELETE, &format!("/containers/{id}?force=1"), None)
            .await;
        r
    }
}

/// The `message` of a Docker error answer.
fn message(bytes: &[u8]) -> String {
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|v| v["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| String::from_utf8_lossy(bytes).trim().to_owned())
}

/// The [`Image`] of an image inspect record, made by `reference`.
pub fn image_of(img: &Value, reference: &str) -> Image {
    let repo = reference.split('@').next().unwrap_or(reference);
    let repo = match repo.rsplit_once(':') {
        // `host:port/name` keeps its port; `name:tag` loses its tag.
        Some((r, tag)) if !tag.contains('/') => r,
        _ => repo,
    };
    let digest = reference
        .split_once('@')
        .map(|(_, d)| d.to_owned())
        .or_else(|| {
            img["RepoDigests"].as_array().and_then(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .find(|d| d.split('@').next() == Some(repo))
                    .and_then(|d| d.split_once('@'))
                    .map(|(_, d)| d.to_owned())
            })
        });
    Image {
        reference: reference.to_owned(),
        id: img["Id"].as_str().unwrap_or_default().to_owned(),
        digest,
        version: img["Config"]["Labels"][LABEL_VERSION]
            .as_str()
            .map(str::to_owned),
    }
}

/// Split the multiplexed stream of an exec or the logs into stdout and
/// stderr. A stream with no frame headers (a TTY) is all stdout.
pub fn demux(bytes: &[u8]) -> (String, String) {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut i = 0;
    while i + 8 <= bytes.len() {
        let kind = bytes[i];
        if kind > 2 || bytes[i + 1..i + 4] != [0, 0, 0] {
            return (String::from_utf8_lossy(bytes).into_owned(), String::new());
        }
        let len =
            u32::from_be_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]) as usize;
        let end = (i + 8 + len).min(bytes.len());
        let chunk = &bytes[i + 8..end];
        if kind == 2 {
            err.extend_from_slice(chunk);
        } else {
            out.extend_from_slice(chunk);
        }
        i = end;
    }
    (
        String::from_utf8_lossy(&out).into_owned(),
        String::from_utf8_lossy(&err).into_owned(),
    )
}

fn strings(v: &Value) -> HashSet<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// A label that compose sets on a container: the project, the service,
/// the working dir, the config files, and the rest. The updater keeps each
/// one, also when the old image has the same label.
fn is_compose_label(key: &str) -> bool {
    key.starts_with("com.docker.compose.")
}

/// Map Compose's host paths through the updater's read-only bind mounts.
/// Refuse paths outside the mounted directories rather than reading a different
/// file with the same name. Mounts come from Docker, not from process env.
fn mounted_path(host: &str, mounts: &Value) -> Res<PathBuf> {
    mounts.as_array().into_iter().flatten()
        .filter_map(|m| {
            let source = Path::new(m["Source"].as_str()?);
            let dest = m["Destination"].as_str()?;
            if !matches!(dest, "/compose" | "/deployment") { return None; }
            let relative = Path::new(host).strip_prefix(source).ok()?;
            if relative.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                return None;
            }
            Some((source.components().count(), Path::new(dest).join(relative)))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, p)| p)
        .ok_or_else(|| "deployment configuration is not mounted; recreate the updater with the current compose.yaml (environment values hidden)".into())
}

fn env_map(env: &Value) -> BTreeMap<String, String> {
    env.as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let (k, v) = v.as_str()?.split_once('=')?;
            Some((k.to_owned(), v.to_owned()))
        })
        .collect()
}

fn resolved_env(config: &Value, service: &str) -> Res<Vec<String>> {
    let env = config["services"][service]["environment"]
        .as_object()
        .ok_or_else(|| "Compose returned no service environment (values hidden)".to_owned())?;
    env.iter()
        .map(|(key, value)| {
            value.as_str().map(|v| format!("{key}={v}")).ok_or_else(|| {
                "Compose returned an unresolved environment value (values hidden)".to_owned()
            })
        })
        .collect()
}

// Compose emits Go durations, including compound and fractional units.
fn stop_grace_seconds(value: &str) -> Option<u64> {
    let mut rest = value.strip_prefix('+').unwrap_or(value);
    let mut seconds = 0.0_f64;
    if rest.is_empty() {
        return None;
    }
    if rest == "0" {
        return Some(0);
    }
    while !rest.is_empty() {
        let end = rest.find(|c: char| !c.is_ascii_digit() && c != '.')?;
        let number: f64 = rest[..end].parse().ok()?;
        rest = &rest[end..];
        let (unit, scale) = [
            ("ns", 1e-9),
            ("us", 1e-6),
            ("µs", 1e-6),
            ("μs", 1e-6),
            ("ms", 1e-3),
            ("s", 1.0),
            ("m", 60.0),
            ("h", 3600.0),
        ]
        .into_iter()
        .find(|(unit, _)| rest.starts_with(unit))?;
        seconds += number * scale;
        rest = &rest[unit.len()..];
    }
    // Round up so Docker's integer seconds never shorten the grace period.
    (seconds.is_finite() && seconds < i64::MAX as f64).then(|| seconds.ceil() as u64)
}

fn with_stop_grace(mut spec: Container, config: &Value, service: &str) -> Res<Container> {
    spec.spec["compose_stop_timeout"] = json!(compose_stop_timeout(config, service)?);
    Ok(spec)
}

fn compose_stop_timeout(config: &Value, service: &str) -> Res<u64> {
    let value = &config["services"][service]["stop_grace_period"];
    let seconds = if value.is_null() {
        DEFAULT_STOP_TIMEOUT
    } else {
        value
            .as_str()
            .and_then(stop_grace_seconds)
            .ok_or_else(|| "invalid Compose stop grace period (values hidden)".to_owned())?
    };
    Ok(seconds)
}

fn stop_timeout(container: &Value, config: &Value, service: &str) -> Res<u64> {
    Ok(compose_stop_timeout(config, service)?
        .max(container["Config"]["StopTimeout"].as_u64().unwrap_or(0)))
}

fn with_environment(spec: &Container, env: Vec<String>, image_config: &Value) -> Container {
    let old = env_map(&spec.spec["container"]["Config"]["Env"]);
    let mut new = env_map(&image_config["Env"]);
    new.extend(env_map(&json!(env)));
    let keys: BTreeSet<_> = old.keys().chain(new.keys()).collect();
    let changed: Vec<_> = keys
        .into_iter()
        .filter(|k| old.get(*k) != new.get(*k))
        .collect();
    tracing::info!(keys = ?changed, "deployment environment changes (values hidden)");
    let mut prepared = spec.clone();
    prepared.spec["changed_env_keys"] = json!(changed);
    prepared.spec["compose_env"] = json!(env);
    prepared
}

fn compose_command() -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("docker");
    // Never inherit the updater's old interpolated service environment.
    cmd.env_clear().env("PATH", "/usr/local/bin:/usr/bin:/bin");
    if let Some(home) = std::env::var_os("HOME") {
        cmd.env("HOME", home);
    }
    cmd
}

/// The body of `POST /containers/create` for a container like `spec`
/// (`{container, image}`: the inspect record and the config of its image)
/// with the image `reference`.
pub fn create_body(spec: &Value, reference: &str) -> Value {
    let c = &spec["container"];
    let img = &spec["image"];
    let mut cfg = c["Config"].as_object().cloned().unwrap_or_default();
    cfg.insert("Image".into(), json!(reference));
    let timeout = spec["compose_stop_timeout"]
        .as_u64()
        .unwrap_or(DEFAULT_STOP_TIMEOUT)
        .max(c["Config"]["StopTimeout"].as_u64().unwrap_or(0));
    cfg.insert("StopTimeout".into(), json!(timeout));
    // The env of the old image: the new image brings its own.
    let image_env = strings(&img["Env"]);
    if let Some(env) = cfg.get_mut("Env").and_then(Value::as_array_mut) {
        env.retain(|e| e.as_str().is_none_or(|s| !image_env.contains(s)));
    }
    // This is a complete Compose service environment, not a patch. In
    // particular, removed keys disappear and explicit image defaults stay.
    if let Some(env) = spec.get("compose_env") {
        cfg.insert("Env".into(), env.clone());
    }
    for key in [
        "Cmd",
        "Entrypoint",
        "WorkingDir",
        "User",
        "StopSignal",
        "Healthcheck",
    ] {
        if cfg.get(key).is_some_and(|v| *v == img[key]) {
            cfg.remove(key);
        }
    }
    if let Some(labels) = cfg.get_mut("Labels").and_then(Value::as_object_mut) {
        labels.retain(|k, v| is_compose_label(k) || img["Labels"].get(k) != Some(v));
    }
    if let Some(vols) = cfg.get_mut("Volumes").and_then(Value::as_object_mut)
        && let Some(iv) = img["Volumes"].as_object()
    {
        vols.retain(|k, _| !iv.contains_key(k));
    }
    let id = c["Id"].as_str().unwrap_or_default();
    if cfg.get("Hostname").and_then(Value::as_str) == Some(short(id)) {
        cfg.remove("Hostname");
    }
    let mut endpoints = Map::new();
    if let Some(nets) = c["NetworkSettings"]["Networks"].as_object() {
        for (name, n) in nets {
            let aliases: Vec<Value> = n["Aliases"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|x| x.as_str() != Some(short(id)))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            let mut ep = Map::new();
            ep.insert("Aliases".into(), Value::Array(aliases));
            for key in ["IPAMConfig", "Links", "DriverOpts"] {
                if !n[key].is_null() {
                    ep.insert(key.into(), n[key].clone());
                }
            }
            endpoints.insert(name.clone(), Value::Object(ep));
        }
    }
    let mut body = cfg;
    body.insert("HostConfig".into(), c["HostConfig"].clone());
    body.insert(
        "NetworkingConfig".into(),
        json!({"EndpointsConfig": endpoints}),
    );
    Value::Object(body)
}

// The service opt-in is an entrypoint interface, not a user daemon argument.
// Normalize it against the destination image, including cross-layout rollback.
fn service_command(body: &mut Value, image: &Value) {
    let mut cmd: Vec<String> = body
        .get("Cmd")
        .unwrap_or(&image["Config"]["Cmd"])
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    if cmd.first().is_some_and(|s| s == "--daemon-service") {
        cmd.remove(0);
    }
    if cmd
        .first()
        .is_some_and(|s| Path::new(s).file_name().is_some_and(|n| n == "terrariumd"))
        && image["Config"]["Labels"]["io.terrarium.hot-swap-layout"] == "1"
    {
        cmd.insert(0, "--daemon-service".into());
    }
    if !cmd.is_empty() {
        body["Cmd"] = json!(cmd);
    }
}

/// The body of a one-off container like `spec` that runs `argv`: the same
/// binds and user, no ports, no network, no restart, no health check, and
/// no compose labels.
pub fn once_body(spec: &Value, reference: &str, argv: &[String]) -> Value {
    let mut body = create_body(spec, reference);
    if let Some(o) = body.as_object_mut() {
        if let Some((first, rest)) = argv.split_first() {
            o.insert("Entrypoint".into(), json!([first]));
            o.insert("Cmd".into(), json!(rest));
        }
        o.insert("Healthcheck".into(), json!({"Test": ["NONE"]}));
        o.remove("ExposedPorts");
        o.remove("Hostname");
        if let Some(labels) = o.get_mut("Labels").and_then(Value::as_object_mut) {
            labels.retain(|k, _| !is_compose_label(k));
        }
        o.insert("NetworkingConfig".into(), json!({"EndpointsConfig": {}}));
        if let Some(h) = o.get_mut("HostConfig").and_then(Value::as_object_mut) {
            h.remove("PortBindings");
            h.insert("NetworkMode".into(), json!("none"));
            h.insert("RestartPolicy".into(), json!({"Name": "no"}));
            h.insert("AutoRemove".into(), json!(false));
        }
    }
    body
}

/// The health of an inspect record.
pub fn health_of(c: &Value) -> Health {
    let st = &c["State"];
    if st["Running"].as_bool() != Some(true) {
        return Health::Exited(format!(
            "{} (exit {}){}",
            st["Status"].as_str().unwrap_or("not running"),
            st["ExitCode"].as_i64().unwrap_or(-1),
            st["Error"]
                .as_str()
                .filter(|e| !e.is_empty())
                .map(|e| format!(": {e}"))
                .unwrap_or_default()
        ));
    }
    match st["Health"]["Status"].as_str() {
        None | Some("healthy") => Health::Healthy,
        Some("unhealthy") => Health::Unhealthy(
            st["Health"]["Log"]
                .as_array()
                .and_then(|l| l.last())
                .and_then(|l| l["Output"].as_str())
                .unwrap_or("the health check failed")
                .trim()
                .to_owned(),
        ),
        Some(_) => Health::Starting,
    }
}

impl Engine for Docker {
    fn can_hot_swap<'a>(&'a self, cur: &'a Container, to: &'a Image) -> BoxFut<'a, Res<bool>> {
        Box::pin(async move {
            let physical = cur.spec["container"]["Image"].as_str().unwrap_or_default();
            let Some(old) = self.inspect_image(physical).await? else {
                return Ok(false);
            };
            let Some(new) = self.inspect_image(&to.id).await? else {
                return Ok(false);
            };
            if !hot_swap_compatible(&old, &new) {
                return Ok(false);
            }
            Ok(self
                .exec(&cur.id, crate::machine::hot_argv("probe", &[]))
                .await?
                .code
                == 0)
        })
    }
    fn target(&self) -> BoxFut<'_, Res<Container>> {
        Box::pin(async move {
            let id = self.target_id().await?;
            self.container(&id).await
        })
    }

    fn prepare<'a>(&'a self, spec: &'a Container, image: &'a Image) -> BoxFut<'a, Res<Container>> {
        Box::pin(async move {
            let (config, service) = self
                .compose_config(&spec.spec["container"], &spec.image.reference)
                .await?;
            let destination = self
                .inspect_image(&image.reference)
                .await
                .map_err(|_| {
                    "cannot inspect destination image environment (values hidden)".to_owned()
                })?
                .ok_or_else(|| "destination image is missing (values hidden)".to_owned())?;
            with_stop_grace(
                with_environment(
                    spec,
                    resolved_env(&config, &service)?,
                    &destination["Config"],
                ),
                &config,
                &service,
            )
        })
    }

    fn image<'a>(&'a self, reference: &'a str) -> BoxFut<'a, Res<Option<Image>>> {
        Box::pin(async move {
            Ok(self
                .inspect_image(reference)
                .await?
                .map(|v| image_of(&v, reference)))
        })
    }

    fn pull<'a>(&'a self, reference: &'a str) -> BoxFut<'a, Res<Image>> {
        Box::pin(async move {
            let (status, bytes) = self
                .call(
                    Method::POST,
                    &format!("/images/create?fromImage={}", enc(reference)),
                    None,
                )
                .await?;
            if !status.is_success() {
                return Err(format!("pull {reference}: {status}: {}", message(&bytes)));
            }
            // The answer is a stream of JSON progress lines; an error is a
            // line with `error`.
            for line in String::from_utf8_lossy(&bytes).lines() {
                if let Ok(v) = serde_json::from_str::<Value>(line)
                    && let Some(e) = v["error"].as_str()
                {
                    return Err(format!("pull {reference}: {e}"));
                }
            }
            let img = self
                .inspect_image(reference)
                .await?
                .ok_or_else(|| format!("pull {reference}: the image is not here after the pull"))?;
            Ok(image_of(&img, reference))
        })
    }

    fn exec<'a>(&'a self, id: &'a str, argv: Vec<String>) -> BoxFut<'a, Res<ExecOut>> {
        Box::pin(async move {
            let made = self
                .ok(
                    Method::POST,
                    &format!("/containers/{}/exec", enc(id)),
                    Some(json!({"AttachStdout": true, "AttachStderr": true, "Cmd": argv})),
                )
                .await?;
            let eid = made["Id"].as_str().unwrap_or_default().to_owned();
            let (status, bytes) = self
                .call(
                    Method::POST,
                    &format!("/exec/{eid}/start"),
                    Some(json!({"Detach": false, "Tty": false})),
                )
                .await?;
            if !status.is_success() {
                return Err(format!("exec: {status}: {}", message(&bytes)));
            }
            let (stdout, stderr) = demux(&bytes);
            let info = self
                .ok(Method::GET, &format!("/exec/{eid}/json"), None)
                .await?;
            Ok(ExecOut {
                code: info["ExitCode"].as_i64().unwrap_or(-1),
                stdout,
                stderr,
            })
        })
    }

    fn exec_create<'a>(&'a self, id: &'a str, argv: Vec<String>) -> BoxFut<'a, Res<String>> {
        Box::pin(async move {
            let made = self
                .ok(
                    Method::POST,
                    &format!("/containers/{}/exec", enc(id)),
                    Some(json!({"AttachStdout":false,"AttachStderr":false,"Cmd":argv})),
                )
                .await?;
            made["Id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| "exec create omitted identity".into())
        })
    }

    fn exec_start<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<()>> {
        Box::pin(async move {
            let (status, bytes) = self
                .call(
                    Method::POST,
                    &format!("/exec/{}/start", enc(id)),
                    Some(json!({"Detach":true,"Tty":false})),
                )
                .await?;
            if status.is_success() {
                Ok(())
            } else {
                Err(format!("exec start: {status}: {}", message(&bytes)))
            }
        })
    }

    fn running_retention_helpers<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<Vec<String>>> {
        Box::pin(async move {
            let container = self.inspect(id).await?;
            let ids = match &container["ExecIDs"] {
                Value::Null => Vec::new(),
                Value::Array(ids) => ids.iter().map(|id| id.as_str().ok_or("invalid container ExecID")).collect::<Result<Vec<_>, _>>()?,
                _ => return Err("invalid container ExecIDs".into()),
            };
            let mut running = Vec::new();
            for id in ids {
                let info = self.ok(Method::GET, &format!("/exec/{}/json", enc(id)), None).await?;
                if !info["Running"].as_bool().ok_or("exec inventory omitted running state")? { continue; }
                let config = &info["ProcessConfig"];
                let entrypoint = config["entrypoint"].as_str().ok_or("exec inventory omitted entrypoint")?;
                let args = config["arguments"].as_array().ok_or("exec inventory omitted arguments")?;
                if entrypoint == "python3" && args.first().and_then(Value::as_str) == Some("/usr/local/lib/terrarium-hot-swap.py") && args.get(1).and_then(Value::as_str) == Some("retain-backups") {
                    running.push(id.to_owned());
                }
            }
            Ok(running)
        })
    }

    fn exec_inspect<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<ExecState>> {
        Box::pin(async move {
            let info = self
                .ok(Method::GET, &format!("/exec/{}/json", enc(id)), None)
                .await?;
            Ok(ExecState {
                running: info["Running"]
                    .as_bool()
                    .ok_or("exec inspect omitted running state")?,
                pid: info["Pid"].as_u64().ok_or("exec inspect omitted PID")?,
                code: info["ExitCode"].as_i64().unwrap_or(-1),
            })
        })
    }

    fn stop<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<()>> {
        Box::pin(async move {
            let c = self.inspect(id).await?;
            let reference = c["Config"]["Image"].as_str().unwrap_or_default();
            let resolved = match self.compose_config(&c, reference).await {
                Ok((config, service)) => stop_timeout(&c, &config, &service),
                Err(error) => Err(error),
            };
            let t = resolved.unwrap_or_else(|_| {
                tracing::warn!(
                    "cannot resolve Compose stop grace period (values hidden); using default"
                );
                c["Config"]["StopTimeout"]
                    .as_u64()
                    .unwrap_or(0)
                    .max(DEFAULT_STOP_TIMEOUT)
            });
            self.ok(
                Method::POST,
                &format!("/containers/{}/stop?t={t}", enc(id)),
                None,
            )
            .await?;
            Ok(())
        })
    }

    fn stop_for_recovery<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<()>> {
        Box::pin(async move {
            // A manual Docker stop suppresses automatic restart. Do not wait
            // the normal 650-second graceful stop on an unverified database.
            self.ok(
                Method::POST,
                &format!("/containers/{}/stop?t=0", enc(id)),
                None,
            )
            .await?;
            Ok(())
        })
    }

    fn remove<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<()>> {
        Box::pin(async move {
            let (status, bytes) = self
                .call(
                    Method::DELETE,
                    &format!("/containers/{}?force=1", enc(id)),
                    None,
                )
                .await?;
            if status.is_success() || status == StatusCode::NOT_FOUND {
                Ok(())
            } else {
                Err(format!("remove {id}: {status}: {}", message(&bytes)))
            }
        })
    }

    fn create<'a>(&'a self, spec: &'a Container, image: &'a Image) -> BoxFut<'a, Res<Container>> {
        Box::pin(async move {
            let mut body = create_body(&spec.spec, &image.reference);
            let destination = self
                .inspect_image(&image.reference)
                .await?
                .ok_or_else(|| "destination image missing before service creation".to_owned())?;
            service_command(&mut body, &destination);
            // Compose records the image ID in a label; a stale one makes
            // the next `compose up` make the container again.
            if let Some(l) = body
                .get_mut("Labels")
                .and_then(Value::as_object_mut)
                .and_then(|l| l.get_mut(LABEL_COMPOSE_IMAGE))
                && !image.id.is_empty()
            {
                *l = json!(image.id);
            }
            let made = self
                .ok(
                    Method::POST,
                    &format!("/containers/create?name={}", enc(&spec.name)),
                    Some(body),
                )
                .await?;
            let id = made["Id"].as_str().unwrap_or_default().to_owned();
            self.ok(Method::POST, &format!("/containers/{id}/start"), None)
                .await?;
            self.container(&id).await
        })
    }

    fn run_once<'a>(
        &'a self,
        spec: &'a Container,
        image: &'a Image,
        mut argv: Vec<String>,
    ) -> BoxFut<'a, Res<ExecOut>> {
        Box::pin(async move {
            // Offline restore must use the requested image's executable, not
            // the volume's current link (which may still select the new daemon).
            if argv.first().is_some_and(|s| s == "terrariumd")
                && let Some(img) = self.inspect_image(&image.id).await?
                && img["Config"]["Labels"]["io.terrarium.hot-swap-layout"] == "1"
            {
                argv[0] = "/opt/terrarium/bundled/bin/terrariumd".into();
            }
            self.run_body(once_body(&spec.spec, &image.reference, &argv))
                .await
        })
    }

    fn health<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<Health>> {
        Box::pin(async move {
            // A container that restarts reads as not running for a moment.
            let c = self.inspect(id).await?;
            if c["State"]["Restarting"].as_bool() == Some(true) {
                tokio::time::sleep(Duration::from_millis(500)).await;
                return Ok(Health::Exited("it restarts (it crashed at start)".into()));
            }
            Ok(health_of(&c))
        })
    }

    fn daemon_failure_since<'a>(
        &'a self,
        id: &'a str,
        since: &'a str,
    ) -> BoxFut<'a, Res<Option<String>>> {
        Box::pin(async move {
            let (status, logs) = self
                .call(
                    Method::GET,
                    &format!(
                        "/containers/{}/logs?stdout=1&stderr=1&tail=64&since={}",
                        enc(id),
                        enc(since)
                    ),
                    None,
                )
                .await
                .map_err(|_| "cannot read daemon startup log (values hidden)".to_owned())?;
            if !status.is_success() {
                return Err("cannot read daemon startup log (values hidden)".into());
            }
            let (stdout, stderr) = demux(&logs);
            Ok(daemon_exit_reason(&stdout, &stderr))
        })
    }
}

/// Only fixed error categories from the last daemon line may leave the
/// Docker log. The updater has no secret store with which to redact arbitrary
/// values, so unknown text must stay hidden rather than using a heuristic.
fn daemon_exit_reason(stdout: &str, stderr: &str) -> Option<String> {
    let lines: Vec<_> = stderr.lines().collect();
    let exit = lines.iter().rposition(|line| {
        line.starts_with("entrypoint: terrariumd ended (")
            || line.starts_with("entrypoint: terrariumd exited with status ")
            || line.starts_with("entrypoint: terrariumd stopped (status ")
    })?;
    let last = lines[..exit]
        .iter()
        .rev()
        .copied()
        .find(|line| !line.trim().is_empty() && !line.starts_with("entrypoint: "))
        .or_else(|| stdout.lines().rev().find(|line| !line.trim().is_empty()))
        .unwrap_or("");
    let category = [
        "FOREIGN KEY constraint failed",
        "UNIQUE constraint failed",
        "NOT NULL constraint failed",
        "database is locked",
        "database disk image is malformed",
        "disk I/O error",
        "no such table",
        "no such column",
    ]
    .into_iter()
    .find(|phrase| last.contains(phrase))
    .unwrap_or("[hidden]");
    Some(format!(
        "new daemon exited during startup; last error: {category} (values hidden)"
    ))
}

/// The release payload is the final layer, by hot-swap-layout contract. A
/// missing label, changed supervisor/base, or protocol break forces replacement.
pub fn hot_swap_compatible(old: &Value, new: &Value) -> bool {
    let labels = |v: &Value| v["Config"]["Labels"].clone();
    let a = labels(old);
    let b = labels(new);
    if a["io.terrarium.hot-swap-layout"] != "1"
        || b["io.terrarium.hot-swap-layout"] != "1"
        || a["io.terrarium.runner-compat-epoch"].as_str().is_none()
        || a["io.terrarium.runner-compat-epoch"] != b["io.terrarium.runner-compat-epoch"]
    {
        return false;
    }
    for key in [
        "Env",
        "Entrypoint",
        "Cmd",
        "User",
        "WorkingDir",
        "Healthcheck",
    ] {
        if old["Config"][key] != new["Config"][key] {
            return false;
        }
    }
    match (
        old["RootFS"]["Layers"].as_array(),
        new["RootFS"]["Layers"].as_array(),
    ) {
        (Some(a), Some(b)) if a.len() > 1 && a.len() == b.len() => {
            a[..a.len() - 1] == b[..b.len() - 1]
        }
        _ => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn service_command_crosses_the_legacy_image_boundary_both_ways() {
        let old = json!({"Config": {"Cmd": ["terrariumd"]}});
        // The real CMD of a hot-swap image (Dockerfile), not the legacy one.
        let new = json!({"Config": {"Cmd": ["--daemon-service", "terrariumd"], "Labels": {"io.terrarium.hot-swap-layout": "1"}}});
        let mut body = create_body(&spec(), "new");
        service_command(&mut body, &new);
        assert_eq!(body["Cmd"], json!(["--daemon-service", "terrariumd"]));
        service_command(&mut body, &old);
        assert_eq!(body["Cmd"], json!(["terrariumd"]));
        body["Cmd"] = json!(["--daemon-service", "terrariumd", "--sandbox", "none"]);
        service_command(&mut body, &old);
        assert_eq!(body["Cmd"], json!(["terrariumd", "--sandbox", "none"]));
    }

    #[test]
    fn hot_swap_requires_identical_runtime_base_and_compatibility_epoch() {
        let a = json!({"Config":{"Labels":{"io.terrarium.hot-swap-layout":"1", "io.terrarium.runner-compat-epoch":"1"}}, "RootFS":{"Layers":["base", "old-payload"]}});
        let mut b = a.clone();
        b["RootFS"]["Layers"][1] = json!("new-payload");
        assert!(hot_swap_compatible(&a, &b));
        b["RootFS"]["Layers"][0] = json!("different-base");
        assert!(!hot_swap_compatible(&a, &b));
        b = a.clone();
        b["Config"]["Labels"]["io.terrarium.runner-compat-epoch"] = json!("2");
        assert!(!hot_swap_compatible(&a, &b));
        assert!(!hot_swap_compatible(&json!({}), &a));
        b = a.clone();
        b["Config"]["Env"] = json!(["NEW=setting"]);
        assert!(!hot_swap_compatible(&a, &b));
    }

    fn spec() -> Value {
        json!({
            "container": {
                "Id": "0123456789abcdef",
                "Name": "/terrarium-terrarium-1",
                "Config": {
                    "Hostname": "0123456789ab",
                    "Image": "terrarium:dev",
                    "Env": ["TERRARIUM_TOKEN=t", "PATH=/usr/bin", "HOME=/home/terrarium"],
                    "Cmd": ["terrariumd"],
                    "Entrypoint": ["/usr/bin/tini", "--", "/usr/local/bin/terrarium-entrypoint"],
                    "User": "terrarium",
                    "Labels": {
                        "com.docker.compose.project": "terrarium",
                        "com.docker.compose.service": "terrarium",
                        "org.opencontainers.image.version": "0.1.0"
                    },
                    "Healthcheck": {"Test": ["CMD", "terrarium", "daemon", "status"]},
                    "StopTimeout": 40
                },
                "HostConfig": {
                    "Binds": ["/srv/t/state:/state:rw"],
                    "PortBindings": {"8080/tcp": [{"HostPort": "8080"}]},
                    "SecurityOpt": ["seccomp={...}"],
                    "RestartPolicy": {"Name": "unless-stopped"}
                },
                "NetworkSettings": {"Networks": {"terrarium_default": {
                    "Aliases": ["terrarium", "0123456789ab"], "IPAMConfig": null}}}
            },
            "image": {
                "Env": ["PATH=/usr/bin", "HOME=/home/terrarium"],
                "Cmd": ["terrariumd"],
                "Entrypoint": ["/usr/bin/tini", "--", "/usr/local/bin/terrarium-entrypoint"],
                "User": "terrarium",
                "Labels": {"org.opencontainers.image.version": "0.1.0"}
            }
        })
    }

    #[test]
    fn recreated_daemon_uses_compose_stop_grace_without_shortening_old_timeout() {
        for (grace, old, expected) in [
            (json!("650s"), 40, 650),
            (json!("10m50s"), 40, 650),
            (Value::Null, 40, 650),
            (json!("650s"), 900, 900),
            (Value::Null, 900, 900),
            (json!("1m0.5s"), 40, 61),
        ] {
            let original = Container {
                id: "old".into(),
                name: "scratch".into(),
                image: image_of(&Value::Null, "old:1"),
                spec: json!({"container": {"Config": {"StopTimeout": old}}}),
            };
            let config = json!({"services": {SERVICE: {"stop_grace_period": grace}}});
            let prepared = with_stop_grace(original, &config, SERVICE).unwrap();
            assert_eq!(create_body(&prepared.spec, "new")["StopTimeout"], expected);
        }
    }

    #[test]
    fn every_stop_uses_the_larger_container_or_compose_grace() {
        for (grace, old, expected) in [
            (json!("650s"), json!(40), 650),
            (json!("650s"), json!(900), 900),
            (Value::Null, json!(40), 650),
            (Value::Null, json!(900), 900),
            (Value::Null, Value::Null, 650),
            (json!("10m50s"), json!(40), 650),
            (json!("1m0.5s"), json!(40), 61),
        ] {
            let container = json!({"Config": {"StopTimeout": old}});
            let config = json!({"services": {SERVICE: {"stop_grace_period": grace}}});
            assert_eq!(
                stop_timeout(&container, &config, SERVICE).unwrap(),
                expected
            );
        }
        assert!(
            stop_timeout(
                &json!({"Config": {"StopTimeout": 40}}),
                &json!({"services": {SERVICE: {"stop_grace_period": "invalid"}}}),
                SERVICE,
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn missing_deployment_mounts_do_not_block_stop() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        for (old, expected) in [(40, 650), (900, 900)] {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("docker.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let server = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        stream.read_exact(&mut byte).await.unwrap();
                        request.push(byte[0]);
                    }
                    let request = String::from_utf8(request).unwrap();
                    let stopping = request.starts_with("POST ");
                    if stopping {
                        assert!(request.starts_with(&format!(
                            "POST /containers/daemon/stop?t={expected} HTTP/1.1\r\n"
                        )));
                    }
                    let body = if request.starts_with("GET /containers/daemon/json ") {
                        json!({"Config": {"StopTimeout": old, "Image": "old:1"}})
                    } else {
                        json!({})
                    }
                    .to_string();
                    stream.write_all(format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    ).as_bytes()).await.unwrap();
                    if stopping {
                        break;
                    }
                }
            });
            let docker = Docker {
                socket,
                target: None,
            };
            tokio::time::timeout(Duration::from_secs(5), docker.stop("daemon"))
                .await
                .unwrap()
                .unwrap();
            server.await.unwrap();
        }
    }

    #[test]
    fn stop_grace_duration_units_and_invalid_values() {
        assert_eq!(stop_grace_seconds("1h2m3s"), Some(3723));
        assert_eq!(stop_grace_seconds("500ms"), Some(1));
        assert_eq!(stop_grace_seconds("0"), Some(0));
        for invalid in ["", "-1s", "650", "1d", "1s junk", "1..2s"] {
            assert_eq!(stop_grace_seconds(invalid), None, "{invalid}");
        }
    }

    #[tokio::test]
    async fn jobs_drain_and_online_backup_cli_and_json_survive_the_docker_socket() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        for (argv, answer) in [
            (
                crate::machine::drain_jobs_argv(Duration::from_secs(12)),
                r#"{"jobs":["job-123"],"timed_out":true}"#,
            ),
            (
                crate::machine::backup_argv(),
                r#"{"path":"/state/backups/pre-pause.zip"}"#,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("docker.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let expected = argv.clone();
            let server = tokio::spawn(async move {
                for step in 0..3 {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") {
                        header.push(stream.read_u8().await.unwrap());
                    }
                    let header = String::from_utf8(header).unwrap();
                    let len = header
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    let mut body = vec![0; len];
                    stream.read_exact(&mut body).await.unwrap();
                    let response = match step {
                        0 => {
                            assert!(header.starts_with("POST /containers/daemon/exec "));
                            let body: Value = serde_json::from_slice(&body).unwrap();
                            assert_eq!(body["Cmd"], json!(expected));
                            r#"{"Id":"exec1"}"#
                        }
                        1 => {
                            assert!(header.starts_with("POST /exec/exec1/start "));
                            answer
                        }
                        _ => {
                            assert!(header.starts_with("GET /exec/exec1/json "));
                            r#"{"ExitCode":0}"#
                        }
                    };
                    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
                }
            });
            let docker = Docker {
                socket,
                target: None,
            };
            let out = tokio::time::timeout(Duration::from_secs(5), docker.exec("daemon", argv))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(out.code, 0);
            let value: Value = serde_json::from_str(&out.stdout).unwrap();
            assert_eq!(value, serde_json::from_str::<Value>(answer).unwrap());
            server.await.unwrap();
        }
    }

    #[test]
    fn daemon_exit_reason_keeps_only_fixed_categories_from_the_last_error() {
        let logs = "entrypoint: started terrariumd (pid 1)\nError: FOREIGN KEY constraint failed: token=do-not-record\nentrypoint: terrariumd ended (status 1) after 0s; starting it again in 1s. The agent runners keep running.\nentrypoint: started terrariumd (pid 2)\n";
        let reason = daemon_exit_reason("", logs).unwrap();
        assert!(reason.contains("FOREIGN KEY constraint failed (values hidden)"));
        assert!(!reason.contains("do-not-record"));
        let unknown = daemon_exit_reason(
            "password=do-not-record",
            "entrypoint: terrariumd ended (status 1)\n",
        )
        .unwrap();
        assert!(unknown.contains("last error: [hidden] (values hidden)"));
        assert!(daemon_exit_reason("", "entrypoint: started terrariumd (pid 2)\n").is_none());
        // Do not report an older error category when the last error differs.
        let changed = daemon_exit_reason("", "FOREIGN KEY constraint failed\npassword=do-not-record\nentrypoint: terrariumd ended (status 1)\n").unwrap();
        assert!(!changed.contains("FOREIGN KEY"));
    }

    #[tokio::test]
    async fn docker_request_deadline_covers_headers_and_the_full_body_and_closes_connection() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for stalled_body in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("docker.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    header.push(stream.read_u8().await.unwrap());
                }
                if stalled_body {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nx")
                        .await
                        .unwrap();
                }
                // Cancellation must also abort the detached hyper connection,
                // not leave it reading an incomplete body indefinitely.
                let mut bytes = Vec::new();
                stream.read_to_end(&mut bytes).await.unwrap();
            });
            let docker = Docker {
                socket,
                target: None,
            };
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                docker.call_until(
                    Method::GET,
                    "/pending",
                    None,
                    tokio::time::Instant::now() + Duration::from_millis(50),
                ),
            )
            .await
            .unwrap();
            assert!(result.unwrap_err().contains("request deadline exceeded"));
            tokio::time::timeout(Duration::from_secs(1), server)
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn uncertain_recovery_stops_without_normal_grace_or_restart() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("docker.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(stream.read_u8().await.unwrap());
            }
            assert!(
                String::from_utf8(header)
                    .unwrap()
                    .starts_with("POST /containers/daemon/stop?t=0 HTTP/1.1")
            );
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let docker = Docker {
            socket,
            target: None,
        };
        tokio::time::timeout(Duration::from_secs(1), docker.stop_for_recovery("daemon"))
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn startup_crash_log_is_scoped_and_redacted_through_the_docker_socket() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("docker.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let since = "2026-11-12T02:00:00.123456789+00:00";
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(stream.read_u8().await.unwrap());
            }
            let header = String::from_utf8(header).unwrap();
            assert!(header.starts_with(&format!(
                "GET /containers/daemon/logs?stdout=1&stderr=1&tail=64&since={} HTTP/1.1",
                enc(since)
            )));
            let text = b"Error: FOREIGN KEY constraint failed: password=do-not-record\nentrypoint: terrariumd ended (status 1) after 0s; starting it again in 1s. The agent runners keep running.\n";
            let mut frame = vec![2, 0, 0, 0];
            frame.extend_from_slice(&(text.len() as u32).to_be_bytes());
            frame.extend_from_slice(text);
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        frame.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(&frame).await.unwrap();
        });
        let docker = Docker {
            socket,
            target: None,
        };
        let failure = tokio::time::timeout(
            Duration::from_secs(3),
            docker.daemon_failure_since("daemon", since),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert!(failure.contains("FOREIGN KEY constraint failed"));
        assert!(!failure.contains("do-not-record"));
        server.await.unwrap();
    }

    #[test]
    fn compose_keeps_home_and_path_but_not_service_environment() {
        let cmd = compose_command();
        let env: BTreeMap<_, _> = cmd.as_std().get_envs().collect();
        assert_eq!(
            env.get(std::ffi::OsStr::new("HOME")).copied().flatten(),
            std::env::var_os("HOME").as_deref()
        );
        assert_eq!(
            env.get(std::ffi::OsStr::new("PATH")).copied().flatten(),
            Some(std::ffi::OsStr::new("/usr/local/bin:/usr/bin:/bin"))
        );
        assert!(env.keys().all(|k| *k == "HOME" || *k == "PATH"));
    }

    #[test]
    fn changed_keys_use_destination_image_defaults_and_compose_overrides() {
        let original = Container {
            id: "old".into(),
            name: "scratch".into(),
            image: image_of(&Value::Null, "old:1"),
            spec: json!({
                "container": {"Config": {"Env": ["CHANGED=old", "SAME=kept", "OVERRIDE=explicit"]}},
                "image": {"Env": ["CHANGED=old", "SAME=old-default", "OVERRIDE=old-default"]}
            }),
        };
        let prepared = with_environment(
            &original,
            vec!["OVERRIDE=explicit".into()],
            &json!({"Env": ["CHANGED=new", "SAME=kept", "OVERRIDE=new-default"]}),
        );
        assert_eq!(prepared.spec["changed_env_keys"], json!(["CHANGED"]));
        assert_eq!(prepared.spec["compose_env"], json!(["OVERRIDE=explicit"]));
    }

    #[test]
    fn fresh_environment_replaces_removed_keys_and_preserves_explicit_image_defaults() {
        let original = Container {
            id: "old".into(),
            name: "scratch".into(),
            image: image_of(&Value::Null, "old:1"),
            spec: spec(),
        };
        let config = json!({"services": {"terrarium": {"environment": {
            "TERRARIUM_MAIL_LOOP_LIMIT": "30", "HOME": "/home/terrarium"
        }}}});
        let prepared = with_environment(
            &original,
            resolved_env(&config, SERVICE).unwrap(),
            &original.spec["image"],
        );
        let body = create_body(&prepared.spec, "new:2");
        assert_eq!(
            env_map(&body["Env"]),
            BTreeMap::from([
                ("HOME".into(), "/home/terrarium".into()),
                ("TERRARIUM_MAIL_LOOP_LIMIT".into(), "30".into()),
            ])
        );
        assert_eq!(
            prepared.spec["changed_env_keys"],
            json!(["TERRARIUM_MAIL_LOOP_LIMIT", "TERRARIUM_TOKEN"])
        );
        assert_eq!(
            create_body(&original.spec, "old:1")["Env"],
            json!(["TERRARIUM_TOKEN=t"])
        );
        assert!(
            resolved_env(
                &json!({"services": {"terrarium": {"environment": {"SECRET": null}}}}),
                SERVICE
            )
            .is_err()
        );
    }

    #[test]
    fn deployment_paths_must_be_inside_the_read_only_mounts() {
        let mounts = json!([
            {"Source": "/srv/install", "Destination": "/deployment"},
            {"Source": "/srv/install/src", "Destination": "/compose"}
        ]);
        assert_eq!(
            mounted_path("/srv/install/src/compose.yaml", &mounts).unwrap(),
            PathBuf::from("/compose/compose.yaml")
        );
        assert_eq!(
            mounted_path("/srv/install/.env", &mounts).unwrap(),
            PathBuf::from("/deployment/.env")
        );
        assert!(mounted_path("/other/.env", &mounts).is_err());
        assert!(mounted_path("/srv/install/../secret", &mounts).is_err());
        assert!(mounted_path("/srv/install/.env", &json!([])).is_err());
    }

    #[test]
    fn the_new_container_keeps_the_settings_and_drops_the_old_image_config() {
        let b = create_body(&spec(), "ghcr.io/x/t@sha256:2");
        assert_eq!(b["Image"], "ghcr.io/x/t@sha256:2");
        assert_eq!(
            b["Env"],
            json!(["TERRARIUM_TOKEN=t"]),
            "the compose env only"
        );
        for k in ["Cmd", "Entrypoint", "User", "Hostname"] {
            assert!(b.get(k).is_none(), "{k} comes from the new image");
        }
        assert_eq!(
            b["Labels"],
            json!({"com.docker.compose.project": "terrarium",
                   "com.docker.compose.service": "terrarium"})
        );
        assert_eq!(
            b["Healthcheck"]["Test"][1], "terrarium",
            "the compose check stays"
        );
        assert_eq!(b["HostConfig"]["Binds"][0], "/srv/t/state:/state:rw");
        assert_eq!(b["HostConfig"]["SecurityOpt"][0], "seccomp={...}");
        assert_eq!(
            b["NetworkingConfig"]["EndpointsConfig"]["terrarium_default"]["Aliases"],
            json!(["terrarium"])
        );
    }

    /// The labels that Docker gives a container made from `body` with
    /// `image`: the image labels, then the labels of the body.
    fn merged_labels(image: &Value, body: &Value) -> Value {
        let mut l = image["Labels"].as_object().cloned().unwrap_or_default();
        for (k, v) in body["Labels"].as_object().cloned().unwrap_or_default() {
            l.insert(k, v);
        }
        Value::Object(l)
    }

    /// The `/containers/json` filter of `target_id` matches these labels.
    fn found_by_compose(labels: &Value, project: &str) -> bool {
        labels[LABEL_PROJECT] == project && labels[LABEL_SERVICE] == SERVICE
    }

    #[test]
    fn two_updates_in_a_row_keep_the_compose_labels_of_a_compose_built_image() {
        // TER-55: `docker compose build` puts the project and service labels on
        // the image as well. The first update dropped them (equal to the old
        // image's), and the second update found no container.
        let compose = json!({
            "com.docker.compose.project": "terrarium-deploy",
            "com.docker.compose.service": "terrarium",
            "com.docker.compose.project.working_dir": "/srv/terrarium-deploy",
            "com.docker.compose.project.config_files": "/srv/terrarium-deploy/compose.yaml",
            "com.docker.compose.container-number": "1",
            "com.docker.compose.oneoff": "False",
            "com.docker.compose.version": "2.29.7"
        });
        let mut s = spec();
        s["container"]["Config"]["Labels"] = compose.clone();
        s["container"]["Config"]["Labels"][LABEL_VERSION] = json!("0.1.0");
        s["image"]["Labels"] = json!({
            "com.docker.compose.project": "terrarium-deploy",
            "com.docker.compose.service": "terrarium",
            "com.docker.compose.version": "2.29.7",
            LABEL_VERSION: "0.1.0"
        });
        assert!(found_by_compose(
            &s["container"]["Config"]["Labels"],
            "terrarium-deploy"
        ));

        // Update 1: to a release image, which has no compose labels.
        let release1 = json!({"Labels": {LABEL_VERSION: "0.1.1"}});
        let b1 = create_body(&s, "ghcr.io/x/t@sha256:1");
        assert_eq!(
            b1["Labels"], compose,
            "every compose label stays, the old version goes"
        );
        let after1 = merged_labels(&release1, &b1);
        assert_eq!(after1[LABEL_VERSION], "0.1.1");
        assert!(found_by_compose(&after1, "terrarium-deploy"));

        // Update 2: from the container that update 1 made.
        let mut s2 = s.clone();
        s2["container"]["Config"]["Labels"] = after1;
        s2["container"]["Config"]["Image"] = json!("ghcr.io/x/t@sha256:1");
        s2["image"] = release1;
        let release2 = json!({"Labels": {LABEL_VERSION: "0.1.2"}});
        let b2 = create_body(&s2, "ghcr.io/x/t@sha256:2");
        assert_eq!(b2["Labels"], compose);
        let after2 = merged_labels(&release2, &b2);
        assert_eq!(after2[LABEL_VERSION], "0.1.2");
        assert!(found_by_compose(&after2, "terrarium-deploy"));
    }

    #[test]
    fn the_one_off_container_has_no_ports_network_restart_or_compose_labels() {
        let argv: Vec<String> = ["terrariumd", "backup", "restore", "/state/backups/b.zip"]
            .map(String::from)
            .to_vec();
        let b = once_body(&spec(), "terrarium:dev", &argv);
        assert_eq!(b["Entrypoint"], json!(["terrariumd"]));
        assert_eq!(
            b["Cmd"],
            json!(["backup", "restore", "/state/backups/b.zip"])
        );
        assert!(b["HostConfig"].get("PortBindings").is_none());
        assert_eq!(b["HostConfig"]["NetworkMode"], "none");
        assert_eq!(b["HostConfig"]["RestartPolicy"]["Name"], "no");
        assert_eq!(b["HostConfig"]["Binds"][0], "/srv/t/state:/state:rw");
        assert_eq!(b["Labels"], json!({}));
        assert_eq!(b["Healthcheck"]["Test"], json!(["NONE"]));
    }

    #[test]
    fn the_image_record_gives_the_digest_and_the_version() {
        let img = json!({
            "Id": "sha256:abc",
            "RepoDigests": ["other/x@sha256:0", "ghcr.io/x/t@sha256:9"],
            "Config": {"Labels": {LABEL_VERSION: "0.3.0"}}
        });
        let i = image_of(&img, "ghcr.io/x/t:0.3.0");
        assert_eq!(i.digest.as_deref(), Some("sha256:9"));
        assert_eq!(i.version.as_deref(), Some("0.3.0"));
        assert_eq!(i.id, "sha256:abc");
        let i = image_of(&img, "ghcr.io/x/t@sha256:7");
        assert_eq!(i.digest.as_deref(), Some("sha256:7"));
        let i = image_of(&json!({"Id": "sha256:1"}), "terrarium:dev");
        assert_eq!(i.digest, None);
        assert_eq!(i.version, None);
        let i = image_of(
            &json!({"RepoDigests": ["localhost:5000/t@sha256:5"]}),
            "localhost:5000/t:1",
        );
        assert_eq!(i.digest.as_deref(), Some("sha256:5"));
    }

    #[test]
    fn the_stream_splits_into_stdout_and_stderr() {
        let mut b = vec![1, 0, 0, 0, 0, 0, 0, 3];
        b.extend_from_slice(b"out");
        b.extend_from_slice(&[2, 0, 0, 0, 0, 0, 0, 3]);
        b.extend_from_slice(b"err");
        assert_eq!(demux(&b), ("out".into(), "err".into()));
        assert_eq!(demux(b"plain tty"), ("plain tty".into(), String::new()));
    }

    #[test]
    fn health_reads_the_state() {
        assert_eq!(
            health_of(&json!({"State": {"Running": true, "Health": {"Status": "healthy"}}})),
            Health::Healthy
        );
        assert_eq!(
            health_of(&json!({"State": {"Running": true, "Health": {"Status": "starting"}}})),
            Health::Starting
        );
        assert_eq!(
            health_of(
                &json!({"State": {"Running": true, "Health": {"Status": "unhealthy",
                "Log": [{"Output": "no daemon\n"}]}}})
            ),
            Health::Unhealthy("no daemon".into())
        );
        assert!(matches!(
            health_of(&json!({"State": {"Running": false, "Status": "exited", "ExitCode": 1}})),
            Health::Exited(e) if e.contains("exit 1")
        ));
        assert_eq!(
            health_of(&json!({"State": {"Running": true}})),
            Health::Healthy
        );
    }
}
