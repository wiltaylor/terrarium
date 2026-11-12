//! B3 scaffolding only. No host verb, registration RPC or enabled admission.
//! Docker inspect is configuration evidence, never proof of an attachment.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use terrarium_core::live_workspace::{Activation, Reply, RuntimeIdentity};

/// Exact versions and topology must be measured again by B5 on any change.
/// There is deliberately no production profile or default approved version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub kernel: String,
    pub engine: String,
    pub runc: String,
    pub seccomp_digest: String,
    pub rootful: bool,
    pub same_mount_namespace: bool,
    pub propagation_proven: bool,
}

impl Profile {
    fn check(&self, pinned: &Self) -> Result<(), Reply> {
        if self != pinned
            || !self.rootful
            || !self.same_mount_namespace
            || !self.propagation_proven
            || [&self.kernel, &self.engine, &self.runc, &self.seccomp_digest]
                .iter().any(|s| s.is_empty())
        {
            return Err(Reply::Unsupported);
        }
        Ok(())
    }
}

/// Future daemon-derived registration, not accepted through generic docker.run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub runtime: RuntimeIdentity,
    pub publisher: String,
    pub profile: Profile,
}

/// This gate cannot be enabled by config, hello, or a caller-provided boolean.
pub fn admit(_activation: Activation) -> Result<(), Reply> {
    Err(Reply::Unsupported)
}

/// Pure create-only argument scaffold. Not callable in production until B4.
/// A closed field set avoids arbitrary flags, volumes and namespace options.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    pub name: String,
    pub image: String,
    pub command: Vec<String>,
}

fn create_flags(req: &Create, registration: &Registration, pinned: &Profile) -> Result<Vec<String>, Reply> {
    registration.profile.check(pinned)?;
    if req.name.is_empty() || req.name.starts_with('-') || req.image.is_empty()
        || req.image.starts_with('-') || !safe_publisher(&registration.publisher)
    {
        return Err(Reply::Unauthorized);
    }
    let mut args: Vec<String> = ["create", "--name", &req.name, "--restart=no",
        "--cap-drop=ALL", "--security-opt=no-new-privileges:true",
        "--network=none", "--ipc=private", "--pid=private",
        "--mount"].into_iter().map(str::to_owned).collect();
    // Admission must reject userns-remap engines. The pinned default seccomp
    // profile must deny namespace creation. No caller security opts are allowed.
    args.push(format!("type=bind,src={},dst=/work,readonly,bind-propagation=rslave", registration.publisher));
    args.extend(["--label".into(), format!("terrarium.workspace.registration={:?}", registration.runtime.registration)]);
    args.push(req.image.clone());
    args.extend(req.command.clone());
    Ok(args)
}

fn safe_publisher(path: &str) -> bool {
    path.starts_with("/var/lib/terrarium-workspace-helper/publishers/")
        && path.strip_prefix("/var/lib/terrarium-workspace-helper/publishers/")
            .is_some_and(|id| id.len() == 26 && id.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()))
}

/// No production caller can obtain a plan while Activation has only Disabled.
pub fn plan_create(activation: Activation, req: &Create, registration: &Registration, pinned: &Profile) -> Result<Vec<String>, Reply> {
    admit(activation)?;
    create_flags(req, registration, pinned)
}

/// Independently verified helper evidence. Kept private so Docker JSON cannot
/// manufacture it. B4 will supply acquisition tied to a pinned runtime and a
/// serialized operation, not deserialize this from the host wire.
pub struct VerifiedScaffold {
    runtime: RuntimeIdentity,
    publisher: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Registration, Create, Value) {
        let profile = Profile { kernel: "synthetic-kernel".into(), engine: "synthetic-engine".into(), runc: "synthetic-runc".into(), seccomp_digest: "synthetic-profile".into(), rootful: true, same_mount_namespace: true, propagation_proven: true };
        let registration: Registration = serde_json::from_value(json!({
            "runtime": {"registration":"01ARZ3NDEKTSV4RRFFQ69G5FAV", "incarnation":"01ARZ3NDEKTSV4RRFFQ69G5FAW", "container_id":"a".repeat(64), "namespace":{"device":1,"inode":2}},
            "publisher":"/var/lib/terrarium-workspace-helper/publishers/01ARZ3NDEKTSV4RRFFQ69G5FAV", "profile":profile
        })).unwrap();
        let req = Create { name: "synthetic".into(), image: "synthetic@sha256:abc".into(), command: vec!["sleep".into(), "600".into()] };
        let row = json!({"Id":"a".repeat(64), "State":{"Running":true}, "HostConfig":{
            "Privileged":false, "RestartPolicy":{"Name":"no"}, "NetworkMode":"none", "IpcMode":"private", "PidMode":"private", "UsernsMode":"", "CapAdd":[], "CapDrop":["ALL"], "SecurityOpt":["no-new-privileges:true"]
        }, "Mounts":[{"Type":"bind", "Source":registration.publisher, "Destination":"/work", "RW":false, "Propagation":"rslave"}]});
        (registration, req, row)
    }
    fn parse(row: Value, r: &Registration) -> Result<Value, Reply> {
        let evidence = VerifiedScaffold { runtime:r.runtime.clone(), publisher:r.publisher.clone() };
        parse_verified_inspect(&json!([row]).to_string(), r, &r.profile, Some(&evidence))
    }
    #[test]
    fn gate_cannot_enable() {
        let (r, req, _) = fixture();
        assert_eq!(plan_create(Activation::Disabled, &req, &r, &r.profile), Err(Reply::Unsupported));
        assert!(serde_json::from_str::<Activation>("\"enabled\"").is_err());
        assert!(!crate::workspace_helper::service::capability().ready);
    }
    #[test]
    fn create_only_closed_configuration() {
        let (r, req, _) = fixture();
        let flags = create_flags(&req, &r, &r.profile).unwrap();
        assert_eq!(flags[0], "create");
        assert!(flags.contains(&format!("type=bind,src={},dst=/work,readonly,bind-propagation=rslave", r.publisher)));
        for key in ["volumes", "workspaces", "privileged", "cap_add", "userns", "pid", "ipc", "network", "security_opt", "restart", "existing_container", "user"] {
            let mut value = serde_json::to_value(&req).unwrap();
            value[key] = json!(true);
            assert!(serde_json::from_value::<Create>(value).is_err(), "{key}");
        }
        for source in ["/", "/work", "/var/run/docker.sock", "/var/lib/terrarium-workspace-helper/publishers/../state", "/var/lib/terrarium-workspace-helper/publishers/ABC,readonly=false"] {
            let mut bad = r.clone(); bad.publisher = source.into();
            assert!(create_flags(&req, &bad, &r.profile).is_err());
        }
    }
    #[test]
    fn profile_changes_refuse() {
        let (r, _, _) = fixture();
        for key in ["kernel", "engine", "runc", "seccomp_digest", "rootful", "same_mount_namespace", "propagation_proven"] {
            let mut value = serde_json::to_value(&r.profile).unwrap();
            value[key] = if value[key].is_boolean() { json!(false) } else { json!("other") };
            let changed: Profile = serde_json::from_value(value).unwrap();
            assert!(changed.check(&r.profile).is_err(), "{key}");
        }
    }
    #[test]
    fn inspect_is_never_attachment_evidence() {
        let (r, _, row) = fixture();
        assert!(parse_verified_inspect(&json!([row.clone()]).to_string(), &r, &r.profile, None).is_err());
        let result = parse(row.clone(), &r).unwrap();
        assert!(result.get("workspace_mounts").is_none());
        assert!(result.get("attached").is_none());
        let mut stale = VerifiedScaffold { runtime:r.runtime.clone(), publisher:r.publisher.clone() };
        stale.runtime.namespace.inode += 1;
        assert!(parse_verified_inspect(&json!([row]).to_string(), &r, &r.profile, Some(&stale)).is_err());
    }
    #[test]
    fn inspect_rejects_unsafe_configuration() {
        let (r, _, row) = fixture();
        for (pointer, value) in [
            ("/HostConfig/Privileged", json!(true)), ("/HostConfig/CapAdd", json!(["SYS_ADMIN"])),
            ("/HostConfig/SecurityOpt", json!(["seccomp=unconfined"])), ("/HostConfig/UsernsMode", json!("host")),
            ("/HostConfig/PidMode", json!("host")), ("/HostConfig/IpcMode", json!("host")),
            ("/HostConfig/NetworkMode", json!("host")), ("/HostConfig/RestartPolicy/Name", json!("always")),
            ("/Mounts/0/Source", json!("/work")), ("/Mounts/0/RW", json!(true)),
            ("/Mounts/0/Propagation", json!("rprivate")), ("/Id", json!("b".repeat(64))),
        ] {
            let mut bad = row.clone(); *bad.pointer_mut(pointer).unwrap() = value;
            assert!(parse(bad, &r).is_err(), "{pointer}");
        }
        let mut foreign = row.clone();
        foreign["Mounts"].as_array_mut().unwrap().push(json!({"Destination":"/var/run/docker.sock"}));
        assert!(parse(foreign, &r).is_err());
    }
}

/// Separate from the legacy rprivate workspace parser. Missing evidence fails
/// closed. This result intentionally never contains workspace_mounts/attached.
pub fn parse_verified_inspect(stdout: &str, registration: &Registration, pinned: &Profile, evidence: Option<&VerifiedScaffold>) -> Result<Value, Reply> {
    registration.profile.check(pinned)?;
    let evidence = evidence.ok_or(Reply::Quarantined)?;
    if evidence.runtime != registration.runtime || evidence.publisher != registration.publisher {
        return Err(Reply::Quarantined);
    }
    let rows: Vec<Value> = serde_json::from_str(stdout).map_err(|_| Reply::Failed)?;
    if rows.len() != 1 { return Err(Reply::Quarantined); }
    let row = &rows[0];
    let h = &row["HostConfig"];
    let mounts = row["Mounts"].as_array().ok_or(Reply::Quarantined)?;
    if row["Id"] != registration.runtime.container_id
        || row["State"]["Running"] != true
        || h["Privileged"] != false
        || h["RestartPolicy"]["Name"] != "no"
        || h["NetworkMode"] != "none"
        || h["IpcMode"] != "private"
        || h["PidMode"] != "private"
        || h["UsernsMode"] != ""
        || h["CapAdd"] != json!([])
        || h["CapDrop"] != json!(["ALL"])
        || h["SecurityOpt"] != json!(["no-new-privileges:true"])
        || mounts.len() != 1
        || mounts[0]["Type"] != "bind"
        || mounts[0]["Source"] != registration.publisher
        || mounts[0]["Destination"] != "/work"
        || mounts[0]["RW"] != false
        || mounts[0]["Propagation"] != "rslave"
    { return Err(Reply::Quarantined); }
    Ok(json!({"id": registration.runtime.container_id, "scaffold_verified": true}))
}
