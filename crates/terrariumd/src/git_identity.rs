//! Agent commit identity and provenance. Push authentication is independent.

use std::collections::BTreeMap;

use crate::api::Shared;
use crate::error::ApiResult;
use crate::executor::sh_quote;
use crate::store::agents;

pub const TRAILERS_FILE: &str = ".terrarium-git-trailers";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub email: String,
}

impl Identity {
    /// Instance settings, deliberately unrelated to the forge credential.
    pub fn from_env() -> Self {
        Self {
            name: std::env::var("TERRARIUM_GIT_NAME").unwrap_or_else(|_| "Terrarium".into()),
            email: std::env::var("TERRARIUM_GIT_EMAIL")
                .unwrap_or_else(|_| "terrarium@localhost".into()),
        }
    }

    pub fn env(&self, env: &mut BTreeMap<String, String>) {
        for (key, value) in [
            ("GIT_AUTHOR_NAME", &self.name),
            ("GIT_COMMITTER_NAME", &self.name),
            ("GIT_AUTHOR_EMAIL", &self.email),
            ("GIT_COMMITTER_EMAIL", &self.email),
        ] {
            env.insert(key.into(), value.clone());
        }
    }
}

/// A checkout fallback. Live sessions and target execs supply current values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub identity: Identity,
    pub trailers: String,
}

pub async fn provenance(state: &Shared, id: &str) -> ApiResult<Provenance> {
    let version = state
        .supervisor
        .harness_runs()
        .into_iter()
        .find(|r| r.agent.to_string() == id)
        .and_then(|r| r.version);
    let id = id.to_owned();
    state
        .db
        .read(move |c| {
            let a = agents::get(c, &id)?;
            let session = agents::current_session(c, &id)?;
            let clean = |s: &str| s.replace(['\r', '\n'], " ");
            let mut trailers = format!(
                "Terrarium-Session: {}\nTerrarium-Agent: {}/{}\nTerrarium-Harness: {} {}\nTerrarium-Model: {}\n",
                session.as_deref().unwrap_or("unknown"),
                clean(&a.agent.project),
                clean(&a.agent.name),
                a.agent.harness,
                clean(version.as_deref().unwrap_or("unknown")),
                clean(a.agent.model.as_deref().unwrap_or("unknown")),
            );
            if let Some(card) = a.current_card {
                trailers.push_str(&format!("Terrarium-Card: {}\n", clean(&card)));
            }
            Ok(Provenance {
                identity: Identity::from_env(),
                trailers,
            })
        })
        .await
}

impl Provenance {
    /// No eval or sourced data. Values are arguments to interpret-trailers.
    pub fn hook(&self) -> String {
        format!(
            "#!/bin/sh\n# terrarium managed provenance hook\nset -eu\nrepo_hook=$(dirname \"$0\")/prepare-commit-msg.repo\nif [ -x \"$repo_hook\" ]; then \"$repo_hook\" \"$@\"; fi\n\
             trailers=${{TERRARIUM_GIT_TRAILERS-}}\n\
             if [ -n \"${{TERRARIUM_GIT_TRAILERS_FILE-}}\" ] && [ -r \"$TERRARIUM_GIT_TRAILERS_FILE\" ]; then\n\
               trailers=$(cat -- \"$TERRARIUM_GIT_TRAILERS_FILE\")\n\
             elif [ -z \"$trailers\" ]; then trailers={}; fi\n\
             printf '%s\\n' \"$trailers\" | while IFS= read -r trailer; do\n\
               [ -n \"$trailer\" ] || continue\n\
               git interpret-trailers --in-place --if-exists replace --if-missing add --trailer \"$trailer\" \"$1\" || exit $?\n\
             done\n",
            sh_quote(&self.trailers)
        )
    }
}

/// Refresh independently of the supervisor event pump, in stored event order.
pub fn spawn_refresh(state: Shared) -> tokio::task::JoinHandle<()> {
    let mut rx = state.db.hub().subscribe();
    tokio::spawn(async move {
        loop {
            let ev = match rx.recv().await {
                Ok(ev) => ev,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("git provenance listener missed {n} events");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            if !matches!(ev.kind.as_str(), "card" | "status" | "session_started") {
                continue;
            }
            let mut ids = vec![];
            if let Some(id) = ev.agent_id {
                ids.push(id);
            }
            if ev.kind == "card"
                && let Some(id) = ev.data["previous"].as_str()
                && id != "operator"
                && !ids.iter().any(|v| v == id)
            {
                ids.push(id.to_owned());
            }
            for id in ids {
                if let Err(e) = refresh(&state, &id).await {
                    tracing::warn!(agent = %id, "cannot refresh git provenance: {e}");
                }
            }
        }
    })
}

/// Refresh the sandbox's current session and card data.
pub async fn refresh(state: &Shared, id: &str) -> ApiResult<()> {
    // Session and card notifications can arrive on separate listeners. Read
    // current state only after the preceding file write has completed.
    static REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = REFRESH_LOCK.lock().await;
    let p = provenance(state, id).await?;
    let id = id.to_owned();
    let workspace = state
        .db
        .read(move |c| Ok(agents::get(c, &id)?.agent.workspace))
        .await?;
    if std::fs::symlink_metadata(&workspace).is_ok_and(|m| m.is_dir()) {
        use std::io::Write as _;
        let save = || -> std::io::Result<()> {
            let mut temp = tempfile::NamedTempFile::new_in(&workspace)?;
            temp.write_all(p.trailers.as_bytes())?;
            temp.persist(workspace.join(TRAILERS_FILE))
                .map_err(|e| e.error)?;
            Ok(())
        };
        save().map_err(|e| crate::error::ApiError::internal(e.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::{Command, Stdio};

    fn git(dir: &Path, args: &[&str], p: &Provenance) -> String {
        let mut env = BTreeMap::new();
        p.identity.env(&mut env);
        let o = Command::new("git")
            .current_dir(dir)
            .args(args)
            .envs(env)
            .env_remove("TERRARIUM_GIT_TRAILERS")
            .env_remove("TERRARIUM_GIT_TRAILERS_FILE")
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout).unwrap()
    }

    #[test]
    fn instance_identity_from_env() {
        if std::env::var_os("TERRARIUM_TEST_IDENTITY").is_some() {
            assert_eq!(
                Identity::from_env(),
                Identity {
                    name: "Instance Bot".into(),
                    email: "bot@example.test".into()
                }
            );
            return;
        }
        assert!(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "git_identity::tests::instance_identity_from_env"])
                .env("TERRARIUM_TEST_IDENTITY", "1")
                .env("TERRARIUM_GIT_NAME", "Instance Bot")
                .env("TERRARIUM_GIT_EMAIL", "bot@example.test")
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn hook_parses_provenance_preserves_release_and_composes_repo_hook() {
        use std::io::Write as _;
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let p = Provenance {
            identity: Identity { name: "Instance Bot".into(), email: "bot@example.test".into() },
            trailers: "Terrarium-Session: 01SESSION\nTerrarium-Agent: proj/agent\nTerrarium-Harness: codex 1.2.3\nTerrarium-Model: model-'$(false)'\nTerrarium-Card: TER-258\n".into(),
        };
        git(d, &["init", "-q", "-b", "main"], &p);
        let hooks = d.join(".git/hooks");
        std::fs::write(hooks.join("prepare-commit-msg"), p.hook()).unwrap();
        std::fs::write(
            hooks.join("prepare-commit-msg.repo"),
            "#!/bin/sh\nprintf 'Repo-Note: yes\\n' >> \"$1\"\n",
        )
        .unwrap();
        for name in ["prepare-commit-msg", "prepare-commit-msg.repo"] {
            std::fs::set_permissions(hooks.join(name), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        git(
            d,
            &[
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "release: test",
                "-m",
                "pre-release: true\nbump: minor",
            ],
            &p,
        );
        let message = git(d, &["show", "-s", "--format=%B"], &p);
        assert!(message.contains("Repo-Note: yes"));
        let mut parse = Command::new("git")
            .args(["interpret-trailers", "--parse"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        parse
            .stdin
            .take()
            .unwrap()
            .write_all(message.as_bytes())
            .unwrap();
        let out = parse.wait_with_output().unwrap();
        assert!(out.status.success());
        let parsed = String::from_utf8(out.stdout).unwrap();
        for trailer in p.trailers.lines() {
            assert!(parsed.contains(trailer), "{parsed}");
        }
        assert!(parsed.contains("pre-release: true"), "{parsed}");
        assert!(parsed.contains("bump: minor"), "{parsed}");
        assert!(!message.to_lowercase().contains("co-authored"));
        assert_eq!(
            git(d, &["show", "-s", "--format=%an <%ae>%n%cn <%ce>"], &p),
            "Instance Bot <bot@example.test>\nInstance Bot <bot@example.test>\n"
        );

        // Merge messages also run prepare-commit-msg, while the daemon's
        // hardened merge jobs intentionally bypass all checkout hooks.
        git(d, &["checkout", "-q", "-b", "feature"], &p);
        git(
            d,
            &["commit", "-q", "--allow-empty", "-m", "feat: branch"],
            &p,
        );
        git(d, &["checkout", "-q", "main"], &p);
        git(
            d,
            &["merge", "-q", "--no-ff", "feature", "-m", "chore: merge"],
            &p,
        );
        let parsed = git(d, &["show", "-s", "--format=%(trailers:only)"], &p);
        assert!(parsed.contains("Terrarium-Session: 01SESSION"), "{parsed}");
    }

    #[test]
    fn target_checkout_installs_identity_and_hook_without_running_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let origin = root.join("origin");
        std::fs::create_dir(&origin).unwrap();
        let p = Provenance {
            identity: Identity { name: "Target Bot".into(), email: "target@example.test".into() },
            trailers: "Terrarium-Session: 01TARGET\nTerrarium-Agent: proj/worker\nTerrarium-Harness: codex unknown\nTerrarium-Model: unknown\n".into(),
        };
        git(&origin, &["init", "-q", "-b", "main"], &p);
        git(
            &origin,
            &["commit", "-q", "--allow-empty", "-m", "init"],
            &p,
        );
        let dest = root.join("dest");
        let cmd = crate::executor::checkout_cmd(
            &format!("file://{}", origin.display()),
            dest.to_str().unwrap(),
            None,
            None,
            None,
        )
        .unwrap();
        let run = || {
            let out = Command::new(&cmd.argv[0])
                .args(&cmd.argv[1..])
                .env_clear()
                .env("PATH", std::env::var("PATH").unwrap())
                .env("HOME", root)
                .env("TERRARIUM_COMMIT_HOOK", p.hook())
                .env("TERRARIUM_COMMIT_NAME", &p.identity.name)
                .env("TERRARIUM_COMMIT_EMAIL", &p.identity.email)
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        };
        run();
        assert_eq!(git(&dest, &["config", "user.name"], &p), "Target Bot\n");
        assert_eq!(
            git(&dest, &["config", "user.email"], &p),
            "target@example.test\n"
        );
        assert!(!dest.join(".git/COMMIT_EDITMSG").exists());
        run(); // An update accepts the config written by the first clone.
        git(
            &dest,
            &["commit", "-q", "--allow-empty", "-m", "feat: target"],
            &p,
        );
        let parsed = git(&dest, &["show", "-s", "--format=%(trailers:only)"], &p);
        assert!(parsed.contains("Terrarium-Session: 01TARGET"), "{parsed}");
        assert!(parsed.contains("Terrarium-Model: unknown"), "{parsed}");
    }
}
