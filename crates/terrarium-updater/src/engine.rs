//! The seams of the updater: the container engine, the registry, and the
//! signature check. The binary uses Docker on docker.sock
//! (`crate::docker`), the registry API (`crate::registry`), and a command
//! (`crate::verify`); the tests use fakes.

use std::future::Future;
use std::pin::Pin;

use crate::protocol::Image;

/// A boxed future, so that the traits are object safe.
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A result with a one-line error for the run record.
pub type Res<T> = Result<T, String>;

/// A container as the engine sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Container {
    pub id: String,
    /// The name, with no leading `/`.
    pub name: String,
    /// The image that the container runs.
    pub image: Image,
    /// The engine's own record of the container (Docker: the inspect
    /// answer, and the config of its image). `create` makes a new
    /// container from it.
    pub spec: serde_json::Value,
}

/// The end of a command in a container.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOut {
    pub code: i64,
    pub stdout: String,
    pub stderr: String,
}

impl ExecOut {
    /// The last line of stderr, else of stdout: the reason of a failure.
    pub fn reason(&self) -> String {
        let pick = |s: &str| {
            s.lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_owned())
        };
        let why = pick(&self.stderr)
            .or_else(|| pick(&self.stdout))
            .unwrap_or_default();
        format!("exit {}: {why}", self.code)
    }
}

/// The health of a container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// It runs; the health check did not pass yet.
    Starting,
    Healthy,
    /// The health check failed its retries.
    Unhealthy(String),
    /// The container does not run.
    Exited(String),
}

/// The container engine.
pub trait Engine: Send + Sync {
    /// The terrarium container (the service `terrarium` of the compose
    /// project of the updater, or `TERRARIUM_UPDATER_TARGET`).
    fn target(&self) -> BoxFut<'_, Res<Container>>;
    /// Resolve deployment settings before any destructive step. Keep the
    /// original container unchanged for recovery after a failed switch.
    fn prepare<'a>(&'a self, spec: &'a Container) -> BoxFut<'a, Res<Container>> {
        Box::pin(async move { Ok(spec.clone()) })
    }
    /// A local image, by ID or reference; `None` when it is not here.
    fn image<'a>(&'a self, reference: &'a str) -> BoxFut<'a, Res<Option<Image>>>;
    /// Pull `reference` (`<repo>@sha256:...`).
    fn pull<'a>(&'a self, reference: &'a str) -> BoxFut<'a, Res<Image>>;
    /// Run `argv` in the running container `id`.
    fn exec<'a>(&'a self, id: &'a str, argv: Vec<String>) -> BoxFut<'a, Res<ExecOut>>;
    /// Stop the container gracefully (its stop timeout).
    fn stop<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<()>>;
    /// Remove the container (and kill it when it runs).
    fn remove<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<()>>;
    /// Make a new container with the settings and the name of `spec` and
    /// the image `image`, and start it.
    fn create<'a>(&'a self, spec: &'a Container, image: &'a Image) -> BoxFut<'a, Res<Container>>;
    /// Run `argv` once in a new container with the mounts of `spec` and the
    /// image `image` (no ports, no restart), wait for its end, and remove
    /// it.
    fn run_once<'a>(
        &'a self,
        spec: &'a Container,
        image: &'a Image,
        argv: Vec<String>,
    ) -> BoxFut<'a, Res<ExecOut>>;
    /// The health of the container `id`.
    fn health<'a>(&'a self, id: &'a str) -> BoxFut<'a, Res<Health>>;
}

/// The image registry.
pub trait Registry: Send + Sync {
    /// The digest (`sha256:...`) of `repository:tag`.
    fn resolve<'a>(&'a self, repository: &'a str, tag: &'a str) -> BoxFut<'a, Res<String>>;
}

/// The signature or provenance check of an image.
pub trait Verifier: Send + Sync {
    /// `gh`, `cosign`, `command`, or `off`.
    fn mode(&self) -> String;
    /// Check `reference` (`<repo>@sha256:...`). `Ok` has one line about
    /// the check. Only called when the mode is not `off`.
    fn verify<'a>(&'a self, reference: &'a str) -> BoxFut<'a, Res<String>>;
}
