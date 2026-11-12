//! Host-file-only Windows identity policy. No request may supply elevation.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Opt-in until native acceptance has been completed on the host.
    #[serde(default)]
    pub enabled: bool,
    /// Exact account spellings allowed to select full tokens. All other
    /// accounts must use ordinary or UAC-filtered tokens.
    #[serde(default)]
    pub accounts: Vec<Account>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub account: String,
    #[serde(default)]
    pub elevated: bool,
}

impl Policy {
    pub fn elevated(&self, account: &str) -> bool {
        self.enabled
            && self
                .accounts
                .iter()
                .any(|a| a.account == account && a.elevated)
    }
}

#[cfg(windows)]
pub fn check_path(
    identity: Option<&terrarium_winapi::Identity>,
    policy: &crate::config::PathPolicy,
    path: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    match identity {
        Some(identity) => identity
            .with_impersonation(|| Ok(policy.check(path)))
            .map_err(|_| "Windows path check impersonation failed".to_owned())?,
        None => policy.check(path),
    }
}

#[cfg(windows)]
pub async fn logon(
    credentials: Option<crate::protocol::WindowsRunAs>,
    policy: &Policy,
) -> Result<Option<std::sync::Arc<terrarium_winapi::Identity>>, crate::verb::BuildError> {
    let Some(credentials) = credentials else {
        return Ok(None);
    };
    if !policy.enabled {
        return Err(crate::verb::BuildError::bad(
            "Windows run-as is disabled in the host file",
        ));
    }
    let full = policy.elevated(&credentials.account);
    tokio::task::spawn_blocking(move || {
        // Wire String storage is not claimed to be securely erased. Drop it
        // immediately after logon, never retain it in a plan, job or replay.
        terrarium_winapi::Identity::logon(&credentials.account, &credentials.password, full)
            .map(|identity| Some(std::sync::Arc::new(identity)))
            .map_err(|e| crate::verb::BuildError::bad(format!("Windows logon failed: {e}")))
    })
    .await
    .map_err(|_| crate::verb::BuildError::bad("Windows logon worker failed"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_enabled_host_entries_elevate() {
        let mut policy = Policy {
            enabled: true,
            accounts: vec![
                Account {
                    account: "DOMAIN\\admin".into(),
                    elevated: true,
                },
                Account {
                    account: "DOMAIN\\ordinary".into(),
                    elevated: false,
                },
            ],
        };
        assert!(policy.elevated("DOMAIN\\admin"));
        for account in [
            "domain\\admin",
            "admin@DOMAIN",
            "DOMAIN\\ordinary",
            "DOMAIN\\other",
        ] {
            assert!(!policy.elevated(account));
        }
        policy.enabled = false;
        assert!(!policy.elevated("DOMAIN\\admin"));
    }

    #[test]
    fn wire_credentials_cannot_choose_elevation() {
        let credentials: crate::protocol::WindowsRunAs = serde_json::from_str(
            r#"{"account":"DOMAIN\\admin","password":"fixture","elevated":true}"#,
        )
        .unwrap();
        assert!(!Policy::default().elevated(&credentials.account));
    }

    #[test]
    fn host_policy_is_strict_and_disabled_by_default() {
        let cfg = crate::config::Config::parse("", std::path::Path::new("fixture")).unwrap();
        assert!(!cfg.windows_run_as.enabled);
        assert!(toml::from_str::<Policy>("enabled=true\nelevation=true").is_err());
    }
}
