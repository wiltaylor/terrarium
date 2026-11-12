//! Protected helper-selected receiver configuration. No production profile,
//! configuration loader or service route is enabled by this module.
use crate::docker::live_workspace::{self, Create, Profile};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use terrarium_core::live_workspace::bootstrap::{Journal, Request};
use terrarium_core::live_workspace::{MAX_FRAME_BYTES, Reply};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Specification {
    image: String,
    command: Vec<String>,
    profile: Profile,
}

/// Fixed field order, no maps and an explicit format/domain version. A change
/// in effective fixed policy changes the digest too, not just config edits.
#[derive(Serialize)]
struct Canonical<'a> {
    format: &'static str,
    specification: &'a Specification,
    security_options: &'static [&'static str],
    receiver_bind_policy: &'static str,
    administrative_parents: [&'static str; 4],
    name_template: &'static str,
    identity_labels: [&'static str; 3],
    host_mount_policy: &'static str,
}

impl Specification {
    pub(super) fn image_reference(&self) -> &str {
        &self.image
    }

    /// The caller reads these bytes from its pinned protected configuration,
    /// never from the bootstrap request or the unprivileged host forwarder.
    pub fn from_config(bytes: &[u8]) -> Result<Self, Reply> {
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(Reply::Failed);
        }
        let spec: Self = serde_json::from_slice(bytes).map_err(|_| Reply::Failed)?;
        spec.valid()?;
        Ok(spec)
    }

    fn valid(&self) -> Result<(), Reply> {
        let (name, digest) = self
            .image
            .split_once("@sha256:")
            .ok_or(Reply::Unauthorized)?;
        if name.is_empty()
            || name.starts_with('-')
            || name.len() > 512
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/._:-".contains(&b))
            || digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.command.is_empty()
            || self.command.len() > 64
            || self
                .command
                .iter()
                .any(|v| v.len() > 4096 || v.as_bytes().contains(&0))
            || self.command.iter().map(String::len).sum::<usize>() > 16384
        {
            return Err(Reply::Unauthorized);
        }
        self.profile.check(&self.profile)
    }

    pub fn digest(&self) -> Result<[u8; 32], Reply> {
        self.valid()?;
        Ok(Sha256::digest(self.canonical_bytes()?).into())
    }

    fn canonical_bytes(&self) -> Result<Vec<u8>, Reply> {
        serde_json::to_vec(&Canonical {
            format: "terrarium.workspace.receiver/v1",
            specification: self,
            security_options: live_workspace::RECEIVER_SECURITY_OPTIONS,
            receiver_bind_policy: live_workspace::RECEIVER_BIND_POLICY,
            administrative_parents: [
                "/var/lib/terrarium-workspace-helper/publishers",
                "/var/lib/terrarium-workspace-helper/staging",
                "/var/lib/terrarium-workspace-helper/registrations",
                "/var/lib/terrarium-workspace-helper/bootstrap",
            ],
            name_template: "terrarium-live-{registration}-{incarnation}",
            identity_labels: ["terrarium.workspace.registration", "terrarium.workspace.runtime", "terrarium.workspace.bootstrap"],
            host_mount_policy: "v1:nonrecursive,private-surrounding,private-before-restrict,nosuid,nodev,exec-preserved,publisher-rw-unique-shared,staging-private",
        }).map_err(|_| Reply::Failed)
    }

    /// Matching protected configuration and a measured profile is necessary,
    /// not sufficient for activation. B5 approval and the disabled gate remain.
    pub fn verify(&self, request: &Request, measured: &Profile) -> Result<(), Reply> {
        Journal::new(request.clone())?;
        if request.configuration != self.digest()? {
            return Err(Reply::Conflict);
        }
        self.profile.check(measured)
    }

    pub fn create_flags(
        &self,
        request: &Request,
        measured: &Profile,
    ) -> Result<Vec<String>, Reply> {
        self.verify(request, measured)?;
        let publisher = format!(
            "/var/lib/terrarium-workspace-helper/publishers/{}",
            request.registration.0
        );
        let create = Create {
            name: format!(
                "terrarium-live-{}-{}",
                request.registration.0, request.incarnation.0
            ),
            image: self.image.clone(),
            command: self.command.clone(),
        };
        live_workspace::bootstrap_create_flags(
            &create,
            &publisher,
            request.registration,
            request.incarnation,
            request.operation,
            &self.profile,
            measured,
        )
    }

    /// Necessary configuration check only, never runtime or mount evidence.
    /// Both inspect bytes and resolved image ID must come from the protected
    /// local Docker inspector, not the request or host forwarder. Callers must
    /// separately pin/recheck the runtime and verify fresh B3 mount evidence.
    pub fn check_inspect_configuration(
        &self,
        request: &Request,
        measured: &Profile,
        container_id: &str,
        resolved_image_id: &str,
        bytes: &[u8],
    ) -> Result<(), Reply> {
        self.verify(request, measured)?;
        let full_id = |s: &str| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        if !full_id(container_id)
            || !resolved_image_id
                .strip_prefix("sha256:")
                .is_some_and(full_id)
        {
            return Err(Reply::Unauthorized);
        }
        if bytes.len() > 256 * 1024 {
            return Err(Reply::Failed);
        }
        let rows: Vec<serde_json::Value> =
            serde_json::from_slice(bytes).map_err(|_| Reply::Failed)?;
        let [row] = rows.as_slice() else {
            return Err(Reply::Quarantined);
        };
        let config = &row["Config"];
        let labels = &config["Labels"];
        let entrypoint_empty = matches!(config.get("Entrypoint"), Some(serde_json::Value::Null))
            || config
                .get("Entrypoint")
                .and_then(serde_json::Value::as_array)
                .is_some_and(Vec::is_empty);
        if row["Id"] != container_id
            || row["Image"] != resolved_image_id
            || row["Name"]
                != format!(
                    "/terrarium-live-{}-{}",
                    request.registration.0, request.incarnation.0
                )
            || config["Image"] != self.image
            || config["Cmd"] != serde_json::json!(self.command)
            || !entrypoint_empty
            || labels["terrarium.workspace.registration"] != request.registration.0.to_string()
            || labels["terrarium.workspace.runtime"] != request.incarnation.0.to_string()
            || labels["terrarium.workspace.bootstrap"] != request.operation.0.to_string()
        {
            return Err(Reply::Quarantined);
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn specification() -> Specification {
        Specification {
            image: format!("synthetic@sha256:{}", "a".repeat(64)),
            command: vec!["/bin/sleep".into(), "infinity".into()],
            profile: Profile {
                kernel: "synthetic".into(),
                engine: "synthetic".into(),
                runc: "synthetic".into(),
                seccomp_digest: "synthetic".into(),
                rootful: true,
                same_mount_namespace: true,
                propagation_proven: true,
                userns_remap: false,
                namespace_creation_blocked: true,
            },
        }
    }

    fn request(spec: &Specification) -> Request {
        serde_json::from_value(serde_json::json!({
            "version": 1, "configuration": spec.digest().unwrap(),
            "operation": "00000000000000000000000001",
            "provenance": {"daemon_instance": "00000000000000000000000002", "caller": "local_operator"},
            "host": "00000000000000000000000003", "target": "synthetic",
            "registration": "00000000000000000000000004",
            "incarnation": "00000000000000000000000005",
            "domain": "00000000000000000000000006"
        })).unwrap()
    }

    #[test]
    fn canonical_v1_binds_effective_policy_and_rejects_old_digest() {
        let spec = specification();
        let bytes = spec.canonical_bytes().unwrap();
        assert!(
            bytes
                .starts_with(b"{\"format\":\"terrarium.workspace.receiver/v1\",\"specification\":")
        );
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body["security_options"],
            serde_json::json!(live_workspace::RECEIVER_SECURITY_OPTIONS)
        );
        assert_eq!(
            body["receiver_bind_policy"],
            live_workspace::RECEIVER_BIND_POLICY
        );
        assert_eq!(body["administrative_parents"].as_array().unwrap().len(), 4);
        let reordered = serde_json::to_vec(&serde_json::to_value(&spec).unwrap()).unwrap();
        assert_eq!(
            Specification::from_config(&reordered).unwrap().digest(),
            spec.digest()
        );
        let mut r = request(&spec);
        r.configuration = Sha256::digest(serde_json::to_vec(&spec).unwrap()).into();
        assert_eq!(spec.verify(&r, &spec.profile), Err(Reply::Conflict));
        r.configuration = [0; 32];
        assert_eq!(spec.verify(&r, &spec.profile), Err(Reply::Unauthorized));
    }

    #[test]
    fn protected_configuration_digest_binds_image_command_and_profile() {
        let spec = specification();
        let r = request(&spec);
        spec.verify(&r, &spec.profile).unwrap();
        for change in 0..3 {
            let mut altered = spec.clone();
            match change {
                0 => altered.image = format!("synthetic@sha256:{}", "b".repeat(64)),
                1 => altered.command.push("changed".into()),
                _ => altered.profile.kernel = "different".into(),
            }
            assert_eq!(altered.verify(&r, &altered.profile), Err(Reply::Conflict));
        }
        let mut runtime = spec.profile.clone();
        runtime.engine = "other".into();
        assert_eq!(spec.verify(&r, &runtime), Err(Reply::Unsupported));
    }

    #[test]
    fn only_fixed_no_pull_create_shape_and_labels_are_emitted() {
        let spec = specification();
        let r = request(&spec);
        let flags = spec.create_flags(&r, &spec.profile).unwrap();
        assert_eq!(flags[0], "create");
        assert!(flags.iter().any(|f| f == "--pull=never"));
        assert!(flags.contains(&format!("terrarium.workspace.bootstrap={}", r.operation.0)));
        assert!(flags.contains(&format!(
            "terrarium.workspace.registration={}",
            r.registration.0
        )));
        assert!(flags.contains(&format!("terrarium.workspace.runtime={}", r.incarnation.0)));
        assert_eq!(
            &flags[flags.len() - 3..],
            &[spec.image.clone(), "/bin/sleep".into(), "infinity".into()]
        );
    }

    #[test]
    fn unpinned_images_unknown_fields_and_oversize_commands_are_refused() {
        let spec = specification();
        for image in ["synthetic:latest", "--privileged", "synthetic@sha256:short"] {
            let mut altered = spec.clone();
            altered.image = image.into();
            assert!(altered.digest().is_err());
        }
        let mut value = serde_json::to_value(&spec).unwrap();
        value["publisher"] = "/caller/path".into();
        assert!(Specification::from_config(&serde_json::to_vec(&value).unwrap()).is_err());
        let mut altered = spec;
        altered.command = vec!["x".repeat(4097)];
        assert!(altered.digest().is_err());
    }

    #[test]
    fn inspect_binds_full_identity_image_command_and_rejects_inherited_entrypoint() {
        let spec = specification();
        let r = request(&spec);
        let id = "a".repeat(64);
        let image = format!("sha256:{}", "b".repeat(64));
        let row = serde_json::json!({
            "Id": id, "Image": image,
            "Name": format!("/terrarium-live-{}-{}", r.registration.0, r.incarnation.0),
            "Config": {"Image": spec.image, "Cmd": spec.command, "Entrypoint": null,
                "Labels": {"terrarium.workspace.registration": r.registration.0.to_string(),
                    "terrarium.workspace.runtime": r.incarnation.0.to_string(),
                    "terrarium.workspace.bootstrap": r.operation.0.to_string()}}
        });
        let check = |rows: serde_json::Value| {
            spec.check_inspect_configuration(
                &r,
                &spec.profile,
                &id,
                &image,
                &serde_json::to_vec(&rows).unwrap(),
            )
        };
        assert_eq!(check(serde_json::json!([row.clone()])), Ok(()));
        for (path, value) in [
            ("/Id", serde_json::json!("c".repeat(64))),
            (
                "/Image",
                serde_json::json!(format!("sha256:{}", "c".repeat(64))),
            ),
            ("/Name", serde_json::json!("/foreign")),
            ("/Config/Image", serde_json::json!("synthetic:latest")),
            ("/Config/Cmd", serde_json::json!(["/bin/other"])),
            ("/Config/Entrypoint", serde_json::json!(["/inherited"])),
            (
                "/Config/Labels/terrarium.workspace.bootstrap",
                serde_json::json!("foreign"),
            ),
            (
                "/Config/Labels/terrarium.workspace.registration",
                serde_json::json!("foreign"),
            ),
            (
                "/Config/Labels/terrarium.workspace.runtime",
                serde_json::json!("foreign"),
            ),
        ] {
            let mut changed = row.clone();
            *changed.pointer_mut(path).unwrap() = value;
            assert_eq!(
                check(serde_json::json!([changed])),
                Err(Reply::Quarantined),
                "{path}"
            );
        }
        let mut missing = row.clone();
        missing["Config"]
            .as_object_mut()
            .unwrap()
            .remove("Entrypoint");
        assert_eq!(check(serde_json::json!([missing])), Err(Reply::Quarantined));
        assert_eq!(check(serde_json::json!([])), Err(Reply::Quarantined));
        assert_eq!(
            check(serde_json::json!([row.clone(), row])),
            Err(Reply::Quarantined)
        );
        assert_eq!(
            spec.check_inspect_configuration(&r, &spec.profile, "short", &image, b"[]"),
            Err(Reply::Unauthorized)
        );
    }
}
