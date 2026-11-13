//! The managed checkout rules shared with `CHECKOUT_SCRIPT` (TER-274
//! D12, D13), written so that they run and are tested on every OS. The
//! Windows host uses them to run its trusted Git for Windows. The daemon
//! test keeps the allowlist equal to the script's `check_config`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::protocol::{CheckoutMode, GitCheckout};

/// `[windows_git]` in the host file: the Git for Windows root. Unset, the
/// host reports no `windows_git_checkout` capability.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<PathBuf>,
}

/// The runtime files audited and held under the root (D10).
pub const RUNTIME_FILES: [&str; 6] = [
    r"cmd\git.exe",
    r"mingw64\bin\git.exe",
    r"mingw64\libexec\git-core\git-remote-https.exe",
    r"bin\sh.exe",
    r"usr\bin\ssh.exe",
    r"usr\bin\false.exe",
];

/// `check_config` of `CHECKOUT_SCRIPT`: keys refused first, in its order.
pub const REJECT_PATTERNS: [&str; 2] = ["remote.*:*", "remote.*/*"];

/// `check_config` of `CHECKOUT_SCRIPT`: the allowed lower-case keys.
pub const ALLOW_PATTERNS: [&str; 38] = [
    "core.repositoryformatversion",
    "core.filemode",
    "core.bare",
    "core.logallrefupdates",
    "core.ignorecase",
    "core.precomposeunicode",
    "core.symlinks",
    "core.autocrlf",
    "core.eol",
    "core.safecrlf",
    "core.quotepath",
    "remote.*.url",
    "remote.*.fetch",
    "remote.*.pushurl",
    "remote.*.push",
    "remote.*.prune",
    "remote.*.tagopt",
    "branch.*.remote",
    "branch.*.merge",
    "branch.*.rebase",
    "branch.*.pushremote",
    "branch.*.description",
    "credential.helper",
    "credential.usehttppath",
    "user.*",
    "advice.*",
    "color.*",
    "push.default",
    "push.autosetupremote",
    "push.followtags",
    "pull.rebase",
    "pull.ff",
    "init.defaultbranch",
    "extensions.objectformat",
    "commit.gpgsign",
    "terrarium.provenance",
    "terrarium.provenancewarnonly",
    "terrarium.repoid",
];

/// Shell `case` glob: `*` matches any run of characters.
fn glob(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((head, tail)) => {
            let Some(rest) = text.strip_prefix(head) else {
                return false;
            };
            (0..=rest.len())
                .filter(|i| rest.is_char_boundary(*i))
                .any(|i| glob(tail, &rest[i..]))
        }
    }
}

/// The first repo config key that the checkout does not allow.
pub fn refused_key<'a>(keys: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    keys.into_iter().find(|key| {
        if key.is_empty() {
            return false;
        }
        let lower = key.to_ascii_lowercase();
        REJECT_PATTERNS.iter().any(|p| glob(p, &lower))
            || !ALLOW_PATTERNS.iter().any(|p| glob(p, &lower))
    })
}

/// The marker line of the managed provenance hook.
pub const COMMIT_MARKER: &str = "# terrarium managed provenance hook";
/// The marker prefix of a managed push hook.
pub const PUSH_MARKER: &str = "# Written by terrariumd";

/// The checks of `CHECKOUT_SCRIPT` on the decoded push hook, by stage.
pub fn check_push_hook(hook: &str) -> Result<(), &'static str> {
    if hook.is_empty() || hook.len() > 24_576 {
        return Err("decode");
    }
    if !hook.bytes().all(|b| b == b'\n' || (0x20..=0x7e).contains(&b)) {
        return Err("bytes");
    }
    if hook.lines().next() != Some("#!/bin/sh") {
        return Err("header");
    }
    if !hook
        .lines()
        .any(|l| l.starts_with("# Written by terrariumd from the repo"))
    {
        return Err("header");
    }
    Ok(())
}

/// Is an existing hook file one of ours, so it may be replaced?
pub fn is_managed(hook_name: &str, content: &str) -> bool {
    match hook_name {
        "prepare-commit-msg" => content.lines().any(|l| l == COMMIT_MARKER),
        "pre-push" => content.lines().any(|l| l.starts_with(PUSH_MARKER)),
        _ => false,
    }
}

/// Request checks of `checkout_cmd`, repeated on the host.
pub fn check_request(req: &GitCheckout) -> Result<(), String> {
    let url = &req.url;
    if url.is_empty()
        || url.starts_with('-')
        || url.chars().any(char::is_control)
        || url.split('/').next().is_some_and(|s| s.contains("::"))
    {
        return Err(format!("repo URL `{url}` cannot be cloned on a target"));
    }
    if let Some(b) = &req.branch
        && (b.is_empty()
            || b.starts_with(['-', '.', '/'])
            || b.contains("..")
            || b.chars()
                .any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c)))
    {
        return Err(format!("bad branch name `{b}`"));
    }
    if req.depth == Some(0) {
        return Err("depth must be at least 1".into());
    }
    if is_ssh_url(url) && req.known_hosts.as_deref().is_none_or(|k| k.trim().is_empty()) {
        return Err("an SSH checkout needs the daemon's pinned known_hosts".into());
    }
    if let CheckoutMode::Managed(m) = &req.mode {
        // TER-1061: every managed control is required, checked before git.
        if m.commit_hook.is_empty()
            || m.name.is_empty()
            || m.email.is_empty()
            || m.repo_id.is_empty()
        {
            return Err("checkout managed hook failed: control".into());
        }
        check_push_hook(&m.push_hook)
            .map_err(|stage| format!("checkout managed hook failed: {stage}"))?;
        if m.commit_hook.contains('\0')
            || [&m.name, &m.email, &m.repo_id]
                .iter()
                .any(|v| v.chars().any(char::is_control))
        {
            return Err("checkout managed hook failed: control".into());
        }
    }
    Ok(())
}

/// Does git reach this URL over SSH? `ssh://`, `git+ssh://`, `ssh+git://`,
/// or the scp form `[user@]host:path` (no `://`, a colon before any slash).
pub fn is_ssh_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    if let Some((scheme, _)) = lower.split_once("://") {
        return matches!(scheme, "ssh" | "git+ssh" | "ssh+git");
    }
    match (url.find(':'), url.find('/')) {
        (Some(colon), Some(slash)) => colon < slash,
        (Some(_), None) => true,
        _ => false,
    }
}

/// The environment names the checkout removes (as `CHECKOUT_SCRIPT`).
pub const UNSET_ENV: [&str; 20] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_NAMESPACE",
    "GIT_CREDENTIAL_HELPER",
    "GIT_TEMPLATE_DIR",
    "GIT_EXEC_PATH",
    "GIT_EXTERNAL_DIFF",
    "GIT_PAGER",
    "GIT_EDITOR",
    "GIT_PROXY_COMMAND",
    "GIT_SSH",
    "GIT_SSH_VARIANT",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
];

/// The credential helper of `CHECKOUT_SCRIPT`. It reads the token from the
/// git child's env only; the token is never in an argument.
pub const TOKEN_HELPER: &str = "!f() { test \"$1\" = get || exit 0; echo \"username=$TERRARIUM_GIT_USERNAME\"; echo \"password=$TERRARIUM_GIT_PASSWORD\"; }; f";

/// The `-c` overrides of `G()` in `CHECKOUT_SCRIPT`, with an empty hooks
/// directory instead of `/dev/null`, and Windows link protections.
pub fn overrides(hooks_dir: &str, safe_directory: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    for setting in [
        format!("core.hooksPath={hooks_dir}"),
        "core.fsmonitor=false".into(),
        "core.untrackedCache=false".into(),
        "core.symlinks=false".into(),
        "core.protectNTFS=true".into(),
        "credential.helper=".into(),
        "protocol.allow=never".into(),
        "protocol.https.allow=always".into(),
        "protocol.http.allow=always".into(),
        "protocol.ssh.allow=always".into(),
        "protocol.file.allow=always".into(),
        "protocol.git.allow=never".into(),
        "protocol.ext.allow=never".into(),
        "protocol.fd.allow=never".into(),
        "protocol.ftp.allow=never".into(),
        "protocol.ftps.allow=never".into(),
        "fetch.recurseSubmodules=false".into(),
        "submodule.recurse=false".into(),
        "gc.auto=0".into(),
        "maintenance.auto=false".into(),
        "merge.verifySignatures=false".into(),
        "advice.detachedHead=false".into(),
        "core.pager=cat".into(),
    ] {
        out.push("-c".into());
        out.push(setting);
    }
    if let Some(safe) = safe_directory {
        out.push("-c".into());
        out.push(format!("safe.directory={safe}"));
    }
    out
}

/// Quote a Windows path for the msys shell that runs `GIT_SSH_COMMAND`.
pub fn sh_quote_path(path: &str) -> Result<String, String> {
    if path.contains(['\'', '\n', '\r']) {
        return Err("path cannot be quoted for the git shell".into());
    }
    Ok(format!("'{}'", path.replace('\\', "/")))
}

/// The `GIT_SSH_COMMAND` of the script, with absolute session paths.
pub fn ssh_command(ssh: &str, known_hosts: &str, key: Option<&str>) -> Result<String, String> {
    let mut cmd = format!(
        "{} -F /dev/null -o UserKnownHostsFile={} -o GlobalKnownHostsFile=/dev/null \
         -o BatchMode=yes -o StrictHostKeyChecking=yes",
        sh_quote_path(ssh)?,
        sh_quote_path(known_hosts)?
    );
    if let Some(key) = key {
        cmd.push_str(&format!(" -i {} -o IdentitiesOnly=yes", sh_quote_path(key)?));
    }
    Ok(cmd)
}

/// The result of a checkout, as `parse_checkout` returns it on Linux.
pub fn result(action: &str, head: &str, branch: &str, path: &str, note: &str) -> serde_json::Value {
    let v = |s: &str| {
        if s.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(s.to_owned())
        }
    };
    serde_json::json!({
        "action": v(action), "head": v(head), "branch": v(branch), "path": v(path), "note": v(note),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ManagedHooks;

    fn push_hook() -> String {
        "#!/bin/sh\n# Written by terrariumd from the repo push_deny list.\nexit 0\n".into()
    }

    #[test]
    fn config_allowlist_matches_the_script_rules() {
        assert_eq!(
            refused_key(["core.bare", "remote.origin.url", "User.Name", "branch.main.merge"]),
            None
        );
        for bad in [
            "core.hookspath",
            "core.fsmonitor",
            "remote.origin.uploadpack",
            "remote.a/b.url",
            "remote.a:b.url",
            "include.path",
            "url.x.insteadof",
            "credential.https://x.helper",
            "filter.lfs.process",
        ] {
            assert_eq!(refused_key(["core.bare", bad]), Some(bad), "{bad}");
        }
        assert!(glob("remote.*.url", "remote.a.b.url"));
        assert!(!glob("user.*", "users.x"));
    }

    #[test]
    fn push_hook_checks_follow_the_script_stages() {
        assert!(check_push_hook(&push_hook()).is_ok());
        assert_eq!(check_push_hook(""), Err("decode"));
        assert_eq!(check_push_hook(&"x".repeat(24_577)), Err("decode"));
        assert_eq!(
            check_push_hook("#!/bin/sh\n# Written by terrariumd from the repo\r\n"),
            Err("bytes")
        );
        assert_eq!(check_push_hook("#!/bin/bash\n# Written by terrariumd from the repo\n"), Err("header"));
        assert_eq!(check_push_hook("#!/bin/sh\nexit 0\n"), Err("header"));
    }

    #[test]
    fn managed_markers_decide_replacement() {
        assert!(is_managed("prepare-commit-msg", "#!/bin/sh\n# terrarium managed provenance hook\n"));
        assert!(!is_managed("prepare-commit-msg", "#!/bin/sh\necho mine\n"));
        assert!(is_managed("pre-push", &push_hook()));
        assert!(!is_managed("pre-push", "#!/bin/sh\nexit 1\n"));
    }

    fn request() -> GitCheckout {
        GitCheckout {
            windows_run_as: None,
            session: 1,
            url: "git@github.com:o/r.git".into(),
            dest: r"C:\Terrarium\work\p\a\r".into(),
            branch: Some("main".into()),
            depth: None,
            mode: CheckoutMode::Managed(ManagedHooks {
                commit_hook: "#!/bin/sh\n# terrarium managed provenance hook\n".into(),
                push_hook: push_hook(),
                name: "a".into(),
                email: "a@x".into(),
                provenance: true,
                warn_only: false,
                repo_id: "r1".into(),
            }),
            credential: None,
            known_hosts: Some("github.com ssh-ed25519 AAAA".into()),
            timeout_secs: None,
        }
    }

    #[test]
    fn requests_are_checked_like_checkout_cmd() {
        assert!(check_request(&request()).is_ok());
        for url in ["", "-u", "ext::sh -c x", "https://x/\n"] {
            let mut r = request();
            r.url = url.into();
            assert!(check_request(&r).is_err(), "{url:?}");
        }
        for branch in ["", "-x", ".x", "a..b", "a b", "a~1", "a\\b"] {
            let mut r = request();
            r.branch = Some(branch.into());
            assert!(check_request(&r).is_err(), "{branch:?}");
        }
        for clear in [0, 1, 2, 3] {
            let mut r = request();
            let CheckoutMode::Managed(m) = &mut r.mode else {
                unreachable!()
            };
            [&mut m.commit_hook, &mut m.name, &mut m.email, &mut m.repo_id][clear].clear();
            assert!(check_request(&r).is_err(), "managed control {clear}");
        }
        let mut r = request();
        let CheckoutMode::Managed(m) = &mut r.mode else {
            unreachable!()
        };
        m.push_hook = "#!/bin/sh\nexit 0\n".into();
        assert!(check_request(&r).is_err(), "unmarked push hook");
        let mut r = request();
        r.depth = Some(0);
        assert!(check_request(&r).is_err());
    }

    #[test]
    fn ssh_urls_need_the_pinned_known_hosts() {
        for url in ["git@github.com:o/r.git", "ssh://git@h/r", "GIT+SSH://h/r", "h:r"] {
            assert!(is_ssh_url(url), "{url}");
            let mut r = request();
            r.url = url.into();
            r.known_hosts = None;
            assert!(check_request(&r).is_err(), "{url} without pin");
            r.known_hosts = Some(" \n".into());
            assert!(check_request(&r).is_err(), "{url} with empty pin");
        }
        for url in ["https://h/r.git", "http://h/r", "file:///C:/r", "./r"] {
            assert!(!is_ssh_url(url), "{url}");
            let mut r = request();
            r.url = url.into();
            r.known_hosts = None;
            assert!(check_request(&r).is_ok(), "{url}");
        }
    }

    /// D16/D17: required fields have no defaults; old capabilities are off.
    #[test]
    fn frames_need_explicit_mode_and_capability_defaults_off() {
        use crate::protocol::{DaemonFrame, decode_control, encode_control};
        let frame = DaemonFrame::GitCheckout(Box::new(request()));
        let text = encode_control(&frame).unwrap();
        assert_eq!(decode_control::<DaemonFrame>(&text).unwrap(), frame);
        let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
        for field in ["mode", "dest", "url", "session"] {
            let mut w = v.clone();
            w.as_object_mut().unwrap().remove(field);
            assert!(
                decode_control::<DaemonFrame>(&w.to_string()).is_err(),
                "{field} must be required"
            );
        }
        v["mode"] = serde_json::json!({"kind": "managed"});
        assert!(decode_control::<DaemonFrame>(&v.to_string()).is_err());
        let caps: crate::protocol::HostCapabilities = serde_json::from_str("{}").unwrap();
        assert!(!caps.windows_git_checkout);
    }

    /// D18: Debug of the request never shows a credential.
    #[test]
    fn request_debug_redacts_credentials() {
        let mut r = request();
        r.credential = Some(crate::protocol::GitCredential::Token {
            username: "u".into(),
            password: "sentinel-token-1".into(),
        });
        r.windows_run_as = Some(crate::protocol::WindowsRunAs {
            account: "D\\a".into(),
            password: "sentinel-password-2".into(),
        });
        let debug = format!("{r:?}");
        assert!(!debug.contains("sentinel-token-1"), "{debug}");
        assert!(!debug.contains("sentinel-password-2"), "{debug}");
        r.credential = Some(crate::protocol::GitCredential::SshKey {
            key: "sentinel-key-3".into(),
        });
        assert!(!format!("{r:?}").contains("sentinel-key-3"));
    }

    #[test]
    fn overrides_carry_no_credential_and_disable_hooks_and_links() {
        let args = overrides(r"C:\Terrarium\git\1\hooks", None);
        assert!(args.contains(&r"core.hooksPath=C:\Terrarium\git\1\hooks".to_owned()));
        assert!(args.contains(&"core.symlinks=false".to_owned()));
        assert!(args.contains(&"protocol.ext.allow=never".to_owned()));
        assert!(args.contains(&"credential.helper=".to_owned()));
        assert_eq!(args.len() % 2, 0);
        assert!(!TOKEN_HELPER.contains("password=s"));
    }

    #[test]
    fn ssh_command_quotes_paths_and_refuses_quotes() {
        let cmd = ssh_command(
            r"C:\Program Files\Git\usr\bin\ssh.exe",
            r"C:\Terrarium\git\1\known_hosts",
            Some(r"C:\Terrarium\git\1\key"),
        )
        .unwrap();
        assert!(cmd.starts_with("'C:/Program Files/Git/usr/bin/ssh.exe' -F /dev/null"));
        assert!(cmd.contains("StrictHostKeyChecking=yes"));
        assert!(!cmd.contains("accept-new"));
        assert!(cmd.contains("-i 'C:/Terrarium/git/1/key' -o IdentitiesOnly=yes"));
        assert!(ssh_command("C:\\it's\\ssh.exe", "k", None).is_err());
    }

    #[test]
    fn results_match_parse_checkout() {
        assert_eq!(
            result("cloned", "abc", "main", r"C:\w", ""),
            serde_json::json!({"action":"cloned","head":"abc","branch":"main","path":"C:\\w","note":null})
        );
    }
}
