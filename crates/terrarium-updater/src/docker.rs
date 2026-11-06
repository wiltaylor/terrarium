//! Docker on `docker.sock` (the Engine API, HTTP/1.1 over the Unix
//! socket). No docker CLI and no compose files: the new container gets the
//! settings of the old one from its inspect record, so the binds, ports,
//! networks, labels (the compose project too), and the seccomp profile stay
//! the same. Only the image changes.
//!
//! The env, the command, the entrypoint, the labels, and the user of the
//! old *image* are not copied: Docker merges the ones of the new image at
//! create. So a new image that changes its own `ENV` or `CMD` takes
//! effect. The compose labels (`com.docker.compose.*`) are the exception:
//! they always stay. An image that `docker compose build` made carries the
//! project and service labels too, and the release image does not, so
//! without them the next update cannot find the container (TER-55).

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request, StatusCode, header};
use hyper_util::rt::TokioIo;
use serde_json::{Map, Value, json};

use crate::engine::{BoxFut, Container, Engine, ExecOut, Health, Res};
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
/// The stop timeout when the container sets none (compose.yaml: 40 s).
pub const DEFAULT_STOP_TIMEOUT: u64 = 40;

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
        let stream = tokio::net::UnixStream::connect(&self.socket)
            .await
            .map_err(|e| format!("cannot reach Docker at {}: {e}", self.socket.display()))?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| format!("Docker handshake: {e}"))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
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
        let image = image_of(&img, &reference);
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

/// The body of `POST /containers/create` for a container like `spec`
/// (`{container, image}`: the inspect record and the config of its image)
/// with the image `reference`.
pub fn create_body(spec: &Value, reference: &str) -> Value {
    let c = &spec["container"];
    let img = &spec["image"];
    let mut cfg = c["Config"].as_object().cloned().unwrap_or_default();
    cfg.insert("Image".into(), json!(reference));
    // The env of the old image: the new image brings its own.
    let image_env = strings(&img["Env"]);
    if let Some(env) = cfg.get_mut("Env").and_then(Value::as_array_mut) {
        env.retain(|e| e.as_str().is_none_or(|s| !image_env.contains(s)));
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
    fn target(&self) -> BoxFut<'_, Res<Container>> {
        Box::pin(async move {
            let id = self.target_id().await?;
            self.container(&id).await
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

    fn stop<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<()>> {
        Box::pin(async move {
            let c = self.inspect(id).await?;
            let t = c["Config"]["StopTimeout"]
                .as_u64()
                .unwrap_or(DEFAULT_STOP_TIMEOUT);
            self.ok(
                Method::POST,
                &format!("/containers/{}/stop?t={t}", enc(id)),
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
        argv: Vec<String>,
    ) -> BoxFut<'a, Res<ExecOut>> {
        Box::pin(async move {
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
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
