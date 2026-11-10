//! The signature check of a release image (`TERRARIUM_UPDATE_VERIFY`).
//!
//! The check runs a program, so that it follows the way the release signs
//! its images (docs/release.md) and can change without a new updater:
//!
//! - `cosign` (the default): `cosign verify --key <TERRARIUM_UPDATE_COSIGN_KEY>
//!   <ref>`. The local release build (`just release::publish`, TER-613)
//!   signs the image digest with the release key pair; the image holds the
//!   public key at [`DEFAULT_COSIGN_KEY`] and the pinned cosign. cosign
//!   checks the signature and its transparency log entry.
//! - `gh`: `gh attestation verify oci://<ref> --repo <owner>/<repo>
//!   --signer-workflow <owner>/<repo>/<workflow> --bundle-from-oci`. The
//!   check of the releases that GitHub Actions made
//!   (`actions/attest-build-provenance`, up to the last ci.yml release).
//!   gh wants a token before it starts, also when it reads the bundle from
//!   the registry: with no `GH_TOKEN` (or `GITHUB_TOKEN`), the updater gives
//!   it a placeholder. A real token works too (higher rate limits).
//! - `cosign-keyless`: `cosign verify` with the identity of the workflows of
//!   the repo (keyless, the GitHub Actions OIDC issuer): for an image of
//!   your own that a workflow signs.
//! - `command:<shell command>`: any command; it gets the image in
//!   `TERRARIUM_UPDATE_REF` and must exit 0 for a good image.
//! - `off`: no check. The run records a skipped `verify` step.
//!
//! None of them works offline: gh and cosign read the Sigstore trust root,
//! the transparency log and the signature over the network. A check that
//! cannot run (no program, no key, no network) fails, and the update stops
//! (fail closed).

use std::time::Duration;

use crate::engine::{BoxFut, ExecOut, Res, Verifier};

/// The env var of the mode.
pub const ENV_VERIFY: &str = "TERRARIUM_UPDATE_VERIFY";
/// The GitHub repo of the releases (`owner/repo`).
pub const ENV_REPO: &str = "TERRARIUM_UPDATE_REPO";
pub const DEFAULT_REPO: &str = "wiltaylor/terrarium";
/// The workflow that signs the releases, in the repo.
pub const ENV_WORKFLOW: &str = "TERRARIUM_UPDATE_WORKFLOW";
pub const DEFAULT_WORKFLOW: &str = ".github/workflows/ci.yml";
/// The public key of the `cosign` mode (a PEM file).
pub const ENV_COSIGN_KEY: &str = "TERRARIUM_UPDATE_COSIGN_KEY";
/// The release public key in the image (docker/cosign.pub).
pub const DEFAULT_COSIGN_KEY: &str = "/opt/terrarium/release/cosign.pub";
/// The placeholder token for gh when the env has none (see the module
/// doc). gh sends it to no GitHub API with `--bundle-from-oci`; a call
/// that needs a real token fails, and the update stops.
pub const GH_PLACEHOLDER_TOKEN: &str = "terrarium-updater-no-token";
/// The OIDC issuer of GitHub Actions.
pub const ACTIONS_ISSUER: &str = "https://token.actions.githubusercontent.com";
/// The longest check.
pub const TIMEOUT: Duration = Duration::from_secs(180);

/// How the updater checks an image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Off,
    /// `cosign verify --key`: the release key pair.
    Cosign,
    /// `cosign verify` keyless, with the identity of the repo's workflows.
    CosignKeyless,
    Gh,
    Command(String),
}

impl Mode {
    /// Parse `TERRARIUM_UPDATE_VERIFY`. Empty is `cosign`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let t = s.trim();
        if let Some(cmd) = t.strip_prefix("command:") {
            if cmd.trim().is_empty() {
                return Err("`command:` needs a command".into());
            }
            return Ok(Self::Command(cmd.trim().to_owned()));
        }
        match t.to_ascii_lowercase().as_str() {
            "" | "cosign" | "on" => Ok(Self::Cosign),
            "cosign-keyless" => Ok(Self::CosignKeyless),
            "gh" => Ok(Self::Gh),
            "off" | "0" | "false" | "no" => Ok(Self::Off),
            other => Err(format!(
                "`{other}`: use cosign, gh, cosign-keyless, off, or command:<shell command>"
            )),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Cosign => "cosign",
            Self::CosignKeyless => "cosign-keyless",
            Self::Gh => "gh",
            Self::Command(_) => "command",
        }
    }
}

/// The certificate identity of the release workflow of `repo`.
pub fn workflow_identity(repo: &str) -> String {
    format!(
        "^https://github\\.com/{}/\\.github/workflows/",
        repo.replace('.', "\\.")
    )
}

/// Where the signature of a release comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signer {
    /// `owner/repo` on GitHub (`gh`, `cosign-keyless`).
    pub repo: String,
    /// The signing workflow in the repo (`gh`).
    pub workflow: String,
    /// The public key file (`cosign`).
    pub key: String,
}

impl Default for Signer {
    fn default() -> Self {
        Self {
            repo: DEFAULT_REPO.into(),
            workflow: DEFAULT_WORKFLOW.into(),
            key: DEFAULT_COSIGN_KEY.into(),
        }
    }
}

/// The program and the arguments of the check of `reference`.
pub fn argv(mode: &Mode, signer: &Signer, reference: &str) -> Option<Vec<String>> {
    let repo = signer.repo.as_str();
    match mode {
        Mode::Off => None,
        Mode::Cosign => Some(vec![
            "cosign".into(),
            "verify".into(),
            "--key".into(),
            signer.key.clone(),
            reference.into(),
        ]),
        Mode::CosignKeyless => Some(vec![
            "cosign".into(),
            "verify".into(),
            "--certificate-identity-regexp".into(),
            workflow_identity(repo),
            "--certificate-oidc-issuer".into(),
            ACTIONS_ISSUER.into(),
            reference.into(),
        ]),
        Mode::Gh => Some(vec![
            "gh".into(),
            "attestation".into(),
            "verify".into(),
            format!("oci://{reference}"),
            "--repo".into(),
            repo.into(),
            "--signer-workflow".into(),
            format!("{repo}/{}", signer.workflow.trim_start_matches('/')),
            "--bundle-from-oci".into(),
        ]),
        Mode::Command(cmd) => Some(vec!["sh".into(), "-c".into(), cmd.clone()]),
    }
}

/// The env that the check gets on top of the updater's own: gh with no
/// prompt and no update notice, and a placeholder token when the env has
/// none (`has_token`).
pub fn check_env(mode: &Mode, has_token: bool) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if *mode == Mode::Gh {
        env.push(("GH_PROMPT_DISABLED".into(), "1".into()));
        env.push(("GH_NO_UPDATE_NOTIFIER".into(), "1".into()));
        if !has_token {
            env.push(("GH_TOKEN".into(), GH_PLACEHOLDER_TOKEN.into()));
        }
    }
    env
}

fn env_has_token() -> bool {
    ["GH_TOKEN", "GITHUB_TOKEN"]
        .iter()
        .any(|k| std::env::var(k).is_ok_and(|v| !v.is_empty()))
}

/// The check that runs a program.
#[derive(Debug, Clone)]
pub struct CommandVerifier {
    pub mode: Mode,
    pub signer: Signer,
}

impl Verifier for CommandVerifier {
    fn mode(&self) -> String {
        self.mode.name().into()
    }

    fn verify<'a>(&'a self, reference: &'a str) -> BoxFut<'a, Res<String>> {
        Box::pin(async move {
            let Some(argv) = argv(&self.mode, &self.signer, reference) else {
                return Ok("not checked".into());
            };
            if self.mode == Mode::Cosign && !std::path::Path::new(&self.signer.key).is_file() {
                return Err(format!(
                    "the cosign public key {} is not a file; set {ENV_COSIGN_KEY}, or choose \
                     another {ENV_VERIFY}",
                    self.signer.key
                ));
            }
            let env = check_env(&self.mode, env_has_token());
            run_check(&argv, &env, self.mode.name(), reference).await
        })
    }
}

/// Run the check `argv` of `reference`; exit 0 accepts the image.
pub async fn run_check(
    argv: &[String],
    env: &[(String, String)],
    mode: &str,
    reference: &str,
) -> Res<String> {
    let Some((program, args)) = argv.split_first() else {
        return Err("no check command".into());
    };
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .env("TERRARIUM_UPDATE_REF", reference)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(TIMEOUT, cmd.output()).await {
        Err(_) => return Err(format!("{program} did not end in {TIMEOUT:?}")),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "`{program}` is not installed in the updater; install it, or choose another \
                 {ENV_VERIFY}"
            ));
        }
        Ok(Err(e)) => return Err(format!("cannot run {program}: {e}")),
        Ok(Ok(o)) => o,
    };
    let o = ExecOut {
        code: i64::from(out.status.code().unwrap_or(-1)),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    };
    if o.code == 0 {
        Ok(format!("{mode} accepted {reference}"))
    } else {
        Err(format!("{mode} refused the image ({})", o.reason()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_mode_parses_and_cosign_is_the_default() {
        assert_eq!(Mode::parse("").unwrap(), Mode::Cosign);
        assert_eq!(Mode::parse("on").unwrap(), Mode::Cosign);
        assert_eq!(Mode::parse("GH").unwrap(), Mode::Gh);
        assert_eq!(Mode::parse("cosign").unwrap(), Mode::Cosign);
        assert_eq!(Mode::parse("Cosign-Keyless").unwrap(), Mode::CosignKeyless);
        assert_eq!(Mode::CosignKeyless.name(), "cosign-keyless");
        assert_eq!(Mode::parse("off").unwrap(), Mode::Off);
        assert_eq!(
            Mode::parse("command: my-check $TERRARIUM_UPDATE_REF").unwrap(),
            Mode::Command("my-check $TERRARIUM_UPDATE_REF".into())
        );
        assert!(Mode::parse("command:").is_err());
        assert!(Mode::parse("maybe").is_err());
    }

    const REF: &str = "ghcr.io/wiltaylor/terrarium@sha256:ab";

    #[test]
    fn gh_checks_the_provenance_of_the_release_workflow_from_the_registry() {
        let a = argv(&Mode::Gh, &Signer::default(), REF).unwrap();
        assert_eq!(
            a.join(" "),
            "gh attestation verify oci://ghcr.io/wiltaylor/terrarium@sha256:ab \
             --repo wiltaylor/terrarium \
             --signer-workflow wiltaylor/terrarium/.github/workflows/ci.yml --bundle-from-oci"
        );
        let c = argv(&Mode::CosignKeyless, &Signer::default(), REF).unwrap();
        assert_eq!(c[0], "cosign");
        assert!(!c.contains(&"--key".to_owned()));
        assert!(c.contains(
            &"^https://github\\.com/wiltaylor/terrarium/\\.github/workflows/".to_owned()
        ));
        assert!(c.contains(&ACTIONS_ISSUER.to_owned()));
        assert_eq!(argv(&Mode::Off, &Signer::default(), "x"), None);
    }

    #[test]
    fn cosign_checks_the_release_key() {
        let a = argv(&Mode::Cosign, &Signer::default(), REF).unwrap();
        assert_eq!(
            a.join(" "),
            "cosign verify --key /opt/terrarium/release/cosign.pub \
             ghcr.io/wiltaylor/terrarium@sha256:ab"
        );
        let s = Signer {
            key: "/keys/mine.pub".into(),
            ..Signer::default()
        };
        assert_eq!(argv(&Mode::Cosign, &s, REF).unwrap()[3], "/keys/mine.pub");
    }

    /// The cosign check with a fake `cosign`: it gets the key and the
    /// digest, its exit decides, and a missing key fails before it runs.
    #[tokio::test]
    async fn the_cosign_check_uses_the_key_and_fails_closed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("cosign.pub");
        std::fs::write(&key, "test key\n").unwrap();
        let log = dir.path().join("log");
        let fake = dir.path().join("cosign");
        let staging = dir.path().join("cosign.tmp");
        std::fs::write(
            &staging,
            format!(
                "#!/bin/sh\necho \"$*\" > {}\n[ \"$4\" = {REF} ]\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&staging, &fake).unwrap();
        let signer = Signer {
            key: key.display().to_string(),
            ..Signer::default()
        };
        let mut a = argv(&Mode::Cosign, &signer, REF).unwrap();
        a[0] = fake.display().to_string();
        let ok = run_check(&a, &[], "cosign", REF).await.unwrap();
        assert_eq!(ok, format!("cosign accepted {REF}"));
        assert_eq!(
            std::fs::read_to_string(&log).unwrap().trim(),
            format!("verify --key {} {REF}", key.display())
        );
        let mut b = argv(&Mode::Cosign, &signer, "ghcr.io/x@sha256:00").unwrap();
        b[0] = fake.display().to_string();
        let e = run_check(&b, &[], "cosign", "ghcr.io/x@sha256:00")
            .await
            .unwrap_err();
        assert!(e.contains("cosign refused the image"), "{e}");
        // No key file: the check fails before cosign runs.
        let v = CommandVerifier {
            mode: Mode::Cosign,
            signer: Signer {
                key: dir.path().join("none.pub").display().to_string(),
                ..Signer::default()
            },
        };
        let e = v.verify(REF).await.unwrap_err();
        assert!(e.contains("is not a file"), "{e}");
        assert!(e.contains(ENV_COSIGN_KEY), "{e}");
    }

    #[test]
    fn gh_gets_a_placeholder_token_only_when_the_env_has_none() {
        let e = check_env(&Mode::Gh, false);
        assert!(e.contains(&("GH_TOKEN".into(), GH_PLACEHOLDER_TOKEN.into())));
        assert!(e.contains(&("GH_PROMPT_DISABLED".into(), "1".into())));
        assert!(
            !check_env(&Mode::Gh, true)
                .iter()
                .any(|(k, _)| k == "GH_TOKEN")
        );
        assert!(check_env(&Mode::Cosign, false).is_empty());
    }

    /// The gh check as the updater runs it, with a fake `gh` that records
    /// its arguments and env: exit 0 accepts, any other exit refuses.
    #[tokio::test]
    async fn the_gh_command_runs_as_built_and_fails_closed() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let fake = dir.path().join("gh");
        let staging = dir.path().join("gh.tmp");
        let mut script = std::fs::File::create(&staging).unwrap();
        let contents = format!(
            "#!/bin/sh\necho \"$* token=$GH_TOKEN ref=$TERRARIUM_UPDATE_REF\" > {}\n\
                 [ \"$3\" = oci://{REF} ]\n",
            log.display()
        );
        script.write_all(contents.as_bytes()).unwrap();
        script
            .set_permissions(std::fs::Permissions::from_mode(0o755))
            .unwrap();
        script.sync_all().unwrap();
        // Publish only after closing the writable handle, before executing it.
        drop(script);
        std::fs::rename(&staging, &fake).unwrap();
        let env = check_env(&Mode::Gh, false);
        let mut a = argv(&Mode::Gh, &Signer::default(), REF).unwrap();
        a[0] = fake.display().to_string();
        let ok = run_check(&a, &env, "gh", REF).await.unwrap();
        assert_eq!(ok, format!("gh accepted {REF}"));
        let line = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            line.trim(),
            format!(
                "attestation verify oci://{REF} --repo wiltaylor/terrarium --signer-workflow \
                 wiltaylor/terrarium/.github/workflows/ci.yml --bundle-from-oci \
                 token={GH_PLACEHOLDER_TOKEN} ref={REF}"
            )
        );
        // Another image: the fake refuses, and so does the check.
        let mut b = argv(&Mode::Gh, &Signer::default(), "ghcr.io/x@sha256:00").unwrap();
        b[0] = fake.display().to_string();
        let e = run_check(&b, &env, "gh", "ghcr.io/x@sha256:00")
            .await
            .unwrap_err();
        assert!(e.contains("gh refused the image"), "{e}");
        // No program: fail closed.
        let e = run_check(&["terrarium-no-such-program".into()], &env, "gh", REF)
            .await
            .unwrap_err();
        assert!(e.contains("is not installed in the updater"), "{e}");
    }

    #[tokio::test]
    async fn a_command_decides() {
        let ok = CommandVerifier {
            mode: Mode::Command("test \"$TERRARIUM_UPDATE_REF\" = good".into()),
            signer: Signer::default(),
        };
        assert!(ok.verify("good").await.is_ok());
        let e = ok.verify("bad").await.unwrap_err();
        assert!(e.contains("command refused the image"), "{e}");
    }
}
