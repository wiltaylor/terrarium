//! The registry API (OCI distribution): the digest of a tag, with an
//! anonymous pull token. GHCR serves a public package so.

use crate::engine::{BoxFut, Registry, Res};

/// The manifest types that the digest may name (an index for a
/// multi-arch image).
const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json";

/// Split `ghcr.io/owner/name` into the host and the name.
pub fn split_repository(repository: &str) -> Res<(String, String)> {
    let (host, name) = repository
        .split_once('/')
        .ok_or_else(|| format!("`{repository}` has no registry host"))?;
    if !host.contains('.') && !host.contains(':') && host != "localhost" {
        return Err(format!("`{repository}` has no registry host"));
    }
    Ok((host.to_owned(), name.to_owned()))
}

/// The `realm`, `service`, and `scope` of a `WWW-Authenticate: Bearer ...`
/// header.
pub fn bearer_challenge(header: &str) -> Option<(String, Vec<(String, String)>)> {
    let rest = header.trim().strip_prefix("Bearer ")?;
    let mut realm = None;
    let mut params = Vec::new();
    for part in split_params(rest) {
        let (k, v) = part.split_once('=')?;
        let v = v.trim().trim_matches('"').to_owned();
        match k.trim() {
            "realm" => realm = Some(v),
            k @ ("service" | "scope") => params.push((k.to_owned(), v)),
            _ => {}
        }
    }
    Some((realm?, params))
}

/// Split on the commas outside quotes.
fn split_params(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// The registry over HTTPS.
#[derive(Debug, Clone)]
pub struct HttpRegistry {
    client: reqwest::Client,
}

impl HttpRegistry {
    pub fn new() -> Res<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("terrarium-updater/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { client })
    }

    async fn head(&self, url: &str, token: Option<&str>) -> Res<reqwest::Response> {
        let mut req = self
            .client
            .head(url)
            .header(reqwest::header::ACCEPT, ACCEPT);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req.send().await.map_err(|e| format!("{url}: {e}"))
    }

    async fn token(&self, challenge: &str) -> Res<String> {
        let (realm, params) =
            bearer_challenge(challenge).ok_or_else(|| format!("bad challenge: {challenge}"))?;
        let resp = self
            .client
            .get(&realm)
            .query(&params)
            .send()
            .await
            .map_err(|e| format!("{realm}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("{realm}: {}", resp.status()));
        }
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        v.get("token")
            .or_else(|| v.get("access_token"))
            .and_then(|t| t.as_str())
            .map(str::to_owned)
            .ok_or_else(|| "the token answer has no token".into())
    }
}

impl Registry for HttpRegistry {
    fn resolve<'a>(&'a self, repository: &'a str, tag: &'a str) -> BoxFut<'a, Res<String>> {
        Box::pin(async move {
            let (host, name) = split_repository(repository)?;
            let url = format!("https://{host}/v2/{name}/manifests/{tag}");
            let mut resp = self.head(&url, None).await?;
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                let challenge = resp
                    .headers()
                    .get(reqwest::header::WWW_AUTHENTICATE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                let token = self.token(&challenge).await?;
                resp = self.head(&url, Some(&token)).await?;
            }
            match resp.status() {
                s if s.is_success() => {}
                reqwest::StatusCode::NOT_FOUND => {
                    return Err(format!("{repository}:{tag} does not exist"));
                }
                s => return Err(format!("{repository}:{tag}: {s}")),
            }
            resp.headers()
                .get("docker-content-digest")
                .and_then(|v| v.to_str().ok())
                .filter(|d| d.starts_with("sha256:"))
                .map(str::to_owned)
                .ok_or_else(|| format!("{repository}:{tag}: the registry sent no digest"))
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_repository_splits_at_the_host() {
        assert_eq!(
            split_repository("ghcr.io/wiltaylor/terrarium").unwrap(),
            ("ghcr.io".into(), "wiltaylor/terrarium".into())
        );
        assert_eq!(
            split_repository("localhost:5000/t").unwrap().0,
            "localhost:5000"
        );
        assert!(split_repository("wiltaylor/terrarium").is_err());
        assert!(split_repository("terrarium").is_err());
    }

    #[test]
    fn the_bearer_challenge_gives_the_token_request() {
        let (realm, params) = bearer_challenge(
            r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:wiltaylor/terrarium:pull""#,
        )
        .unwrap();
        assert_eq!(realm, "https://ghcr.io/token");
        assert_eq!(
            params,
            [
                ("service".to_owned(), "ghcr.io".to_owned()),
                (
                    "scope".to_owned(),
                    "repository:wiltaylor/terrarium:pull".to_owned()
                )
            ]
        );
        assert!(bearer_challenge("Basic realm=x").is_none());
    }
}
