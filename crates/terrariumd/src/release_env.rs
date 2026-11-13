//! The environment keys of this release's `compose.yaml` (TER-1209).
//!
//! The self-updater never changes the deployed compose file. A release that
//! adds a key to the `environment` of the `terrarium` service does not
//! reach the container until the operator adds the key to the deployed
//! compose file (or its `.env`). Before an update, the updater runs
//! `terrariumd release-env-keys` in a one-off container of the new image
//! and compares the key names with the resolved deployed environment
//! (docs/api.md §34.3).
//!
//! A release marks a key as required with the end-of-line comment
//! `# update:required` on the key's line in `compose.yaml`. The updater
//! refuses an update when the deployment lacks a required key, and warns
//! for any other missing key.
//!
//! Only key names leave this module, never values.

use serde::Serialize;

/// The `compose.yaml` of this build.
pub const COMPOSE: &str = include_str!("../../../compose.yaml");
/// The daemon service in `compose.yaml`.
pub const SERVICE: &str = "terrarium";
/// The end-of-line comment that marks a key as required.
pub const REQUIRED_MARK: &str = "# update:required";

/// The key names of a service environment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReleaseEnv {
    pub service: String,
    /// Every key, in file order.
    pub keys: Vec<String>,
    /// The keys marked `# update:required`.
    pub required: Vec<String>,
    /// The 1-based line numbers of environment entries that the parser
    /// cannot read (no key name). They are not checked, so the updater
    /// says so. Line numbers only, never the line.
    pub skipped: Vec<usize>,
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn is_key(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The key of one environment entry: `KEY: value` (map form) or
/// `- KEY=value` / `- KEY` (list form). Quotes around the key are allowed.
fn entry_key(body: &str) -> Option<&str> {
    let raw = match body.strip_prefix("- ") {
        Some(item) => item
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .split('=')
            .next()?,
        None => body.split_once(':')?.0,
    };
    let key = raw.trim().trim_matches(|c| c == '"' || c == '\'');
    is_key(key).then_some(key)
}

/// The environment keys of `service` in a compose file. A line parser for
/// the block style that Terrarium's `compose.yaml` uses: `services:` at
/// the top level, the service one level in, its `environment:` one more
/// level in, and one entry per line below it.
pub fn service_env(compose: &str, service: &str) -> ReleaseEnv {
    let mut out = ReleaseEnv {
        service: service.to_owned(),
        ..ReleaseEnv::default()
    };
    let mut in_services = false;
    // The indent of the service names under `services:`.
    let mut services_indent: Option<usize> = None;
    let mut in_service = false;
    // The indent of the service's own keys (`image:`, `environment:`).
    let mut field_indent: Option<usize> = None;
    let mut env_indent: Option<usize> = None;
    // The indent of the entries; a deeper line continues an entry.
    let mut entry_indent: Option<usize> = None;
    for (n, line) in compose.lines().enumerate() {
        let body = line.trim();
        if body.is_empty() || body.starts_with('#') {
            continue;
        }
        let ind = indent(line);
        if ind == 0 {
            in_services = body == "services:";
            services_indent = None;
            in_service = false;
            env_indent = None;
            entry_indent = None;
            continue;
        }
        if !in_services {
            continue;
        }
        if let Some(e) = env_indent {
            if ind > e {
                if *entry_indent.get_or_insert(ind) == ind {
                    match entry_key(body) {
                        Some(key) => {
                            out.keys.push(key.to_owned());
                            if body.ends_with(REQUIRED_MARK) {
                                out.required.push(key.to_owned());
                            }
                        }
                        None => out.skipped.push(n + 1),
                    }
                }
                continue;
            }
            env_indent = None;
            entry_indent = None;
        }
        let names = *services_indent.get_or_insert(ind);
        if ind <= names {
            in_service = body.ends_with(':') && body.trim_end_matches(':') == service;
            field_indent = None;
            continue;
        }
        if in_service && *field_indent.get_or_insert(ind) == ind && body == "environment:" {
            env_indent = Some(ind);
        }
    }
    out
}

/// The environment keys of the daemon service in this build's
/// `compose.yaml`.
pub fn release_env() -> ReleaseEnv {
    service_env(COMPOSE, SERVICE)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
name: x
services:
  front:
    networks:
      terrarium:
        environment:
          NESTED: 1
    environment:
      FRONT_ONLY: 1
  terrarium:
    image: x
    # a comment
    environment:
      TERRARIUM_TOKEN: ${TERRARIUM_TOKEN:?secret-sentinel}
      TERRARIUM_NEW: ${TERRARIUM_NEW:-on}  # update:required
      \"QUOTED_KEY\": 1

      PLAIN: plain-value
      BLOCK: |
        continued: line
      not a key line
    volumes:
      - NOT_A_KEY=1
  other:
    environment:
      - LIST_FORM=1
volumes:
  data:
";

    #[test]
    fn reads_only_the_service_keys_and_the_required_marks() {
        let env = service_env(SAMPLE, "terrarium");
        assert_eq!(
            env.keys,
            [
                "TERRARIUM_TOKEN",
                "TERRARIUM_NEW",
                "QUOTED_KEY",
                "PLAIN",
                "BLOCK"
            ]
        );
        assert_eq!(env.required, ["TERRARIUM_NEW"]);
        // The continuation of BLOCK is no entry; the bad line is reported
        // by number.
        assert_eq!(env.skipped, [21]);
        let json = serde_json::to_string(&env).unwrap();
        assert!(!json.contains("sentinel") && !json.contains("plain-value"));
    }

    #[test]
    fn reads_the_list_form() {
        let env = service_env(SAMPLE, "other");
        assert_eq!(env.keys, ["LIST_FORM"]);
        assert!(env.required.is_empty());
        assert!(service_env(SAMPLE, "missing").keys.is_empty());
    }

    #[test]
    fn the_embedded_compose_has_the_daemon_keys() {
        let env = release_env();
        for key in [
            "TERRARIUM_TOKEN",
            "TERRARIUM_SELF_UPDATE",
            "TERRARIUM_UPDATER_SOCKET",
        ] {
            assert!(env.keys.iter().any(|k| k == key), "{key}");
        }
        // The updater service's own keys are not the daemon's.
        assert!(!env.keys.iter().any(|k| k == "TERRARIUM_UPDATE_VERIFY"));
        for key in &env.required {
            assert!(env.keys.contains(key));
        }
        assert!(
            env.skipped.is_empty(),
            "unreadable lines: {:?}",
            env.skipped
        );
    }
}
