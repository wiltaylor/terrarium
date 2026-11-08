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
            "#!/bin/sh\nset -eu\nrepo_hook=$(dirname \"$0\")/prepare-commit-msg.repo\nif [ -x \"$repo_hook\" ]; then \"$repo_hook\" \"$@\"; fi\n\
             trailers=${{TERRARIUM_GIT_TRAILERS-}}\n\
             if [ -n \"${{TERRARIUM_GIT_TRAILERS_FILE-}}\" ]; then\n\
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

/// Refresh the sandbox's session data after the store records a session event.
pub async fn refresh(state: &Shared, id: &str) -> ApiResult<()> {
    let p = provenance(state, id).await?;
    let id = id.to_owned();
    let workspace = state.db.read(move |c| Ok(agents::get(c, &id)?.agent.workspace)).await?;
    if workspace.is_dir() {
        use std::io::Write as _;
        let save = || -> std::io::Result<()> {
            let mut temp = tempfile::NamedTempFile::new_in(&workspace)?;
            temp.write_all(p.trailers.as_bytes())?;
            temp.persist(workspace.join(TRAILERS_FILE)).map_err(|e| e.error)?;
            Ok(())
        };
        save().map_err(|e| crate::error::ApiError::internal(e.to_string()))?;
    }
    Ok(())
}
