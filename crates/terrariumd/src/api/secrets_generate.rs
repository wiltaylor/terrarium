//! `POST /api/secrets/{scope}/{name}/generate` (PLAN §16b.6a, docs/api.md
//! §5.2): make a new SSH key pair in the store. The private key never
//! leaves the daemon; the answer is the secret info with its
//! `public_key` and `fingerprint`.
//!
//! The rights are the rights of a secret create (`manage_secrets`). The
//! audit row is `secret.put` with `"generated": true`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Map, json};

use super::Shared;
use crate::actor::Actor;
use crate::error::{ApiError, ApiResult};
use crate::policy;
use crate::secrets::ssh::{self, KeyType};
use crate::secrets::{self, Kind, NewSecret, PutMode, SecretInfo, SecretInput};

pub fn routes() -> Router<Shared> {
    Router::new().route("/secrets/{scope}/{name}/generate", post(generate))
}

/// The body of a generate. Each field is optional.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerateBody {
    /// `ed25519` (the default) or `rsa`.
    #[serde(default, rename = "type")]
    pub key_type: Option<KeyType>,
    /// RSA only: 3072 (the default) or 4096.
    #[serde(default)]
    pub bits: Option<usize>,
    /// The comment of the public key. Default: `terrarium:<scope>/<name>`.
    #[serde(default)]
    pub comment: Option<String>,
    /// Replace a secret that exists (the old key is gone). Default: `409`.
    #[serde(default)]
    pub replace: bool,
}

async fn generate(
    State(state): State<Shared>,
    actor: Actor,
    Path((scope, name)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<SecretInfo>)> {
    policy::manage_secrets(&actor, &scope)?;
    // No body, or `{}`: an ed25519 key.
    let b: GenerateBody = if body.iter().all(u8::is_ascii_whitespace) {
        GenerateBody::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("bad body: {e}")))?
    };
    let key_type = b.key_type.unwrap_or(KeyType::Ed25519);
    let comment = b
        .comment
        .clone()
        .unwrap_or_else(|| format!("terrarium:{scope}/{name}"));
    // An RSA key takes a moment: off the async threads.
    let bits = b.bits;
    let private = tokio::task::spawn_blocking(move || ssh::generate(key_type, bits, &comment))
        .await
        .map_err(|e| ApiError::internal(format!("the key task failed: {e}")))??;
    let mut fields = Map::new();
    fields.insert("private_key".into(), private.as_str().into());
    let input = SecretInput {
        fields,
        secret_fields: Vec::new(),
    };
    drop(private);
    let key = secrets::key(&state.state_dir)?;
    let by = actor.label();
    let mode = if b.replace {
        PutMode::Upsert
    } else {
        PutMode::Create
    };
    let (info, created) = state
        .db
        .write(actor.label(), move |t| {
            let (info, created) = secrets::put(
                t,
                &key,
                mode,
                &NewSecret {
                    scope: &scope,
                    name: &name,
                    kind: Kind::SshKey,
                    input: &input,
                    by: &by,
                    strict: true,
                },
            )?;
            t.audit(
                "secret.put",
                &format!("{scope}/{name}"),
                secrets::scope_project(&scope),
                json!({
                    "scope": scope, "name": name, "kind": Kind::SshKey, "created": created,
                    "generated": true, "type": key_type, "bits": bits,
                    "fingerprint": info.fields.get("fingerprint"),
                    "plain_fields": info.fields.keys().collect::<Vec<_>>(),
                    "secret_fields": info.secret_fields,
                }),
            )?;
            Ok((info, created))
        })
        .await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(info)))
}
