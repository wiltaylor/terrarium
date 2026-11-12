//! Inactive data codecs only. No connection, mutation, approval or loaded-filter proof.
//! A future collector must bound allocation and enforce the absolute deadline
//! while reading, not merely call this decoder after an unbounded read.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::Deserialize;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const INVENTORY_REQUEST: &[u8] = b"GET /v1.47/containers/json?all=1&size=0 HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nConnection: close\r\n\r\n";
pub const DEADLINE: Duration = Duration::from_secs(5);
pub const MAX_HEADERS: usize = 32 * 1024;
pub const MAX_BODY: usize = 4 * 1024 * 1024;
const MAX_RECORDS: usize = 4096;
const MAX_STRING: usize = 4096;
const MAX_POLICY: usize = 256 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub struct Refused;

/// JSON value deserialization that never silently overwrites duplicate keys,
/// including unknown/nested fields. Serde's recursion limit remains enabled.
struct Unique(Value);

impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Unique;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("duplicate-free JSON")
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Unique, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| Unique(Value::Number(n)))
                    .ok_or_else(|| E::custom("number"))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Unique, A::Error> {
                let mut values = Vec::new();
                while let Some(Unique(v)) = a.next_element()? {
                    values.push(v);
                }
                Ok(Unique(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Unique, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some(k) = a.next_key::<String>()? {
                    if values.contains_key(&k) {
                        return Err(serde::de::Error::custom("duplicate key"));
                    }
                    let Unique(v) = a.next_value()?;
                    values.insert(k, v);
                }
                Ok(Unique(Value::Object(values)))
            }
        }
        d.deserialize_any(V)
    }
}

fn json(bytes: &[u8]) -> Result<Value, Refused> {
    serde_json::from_slice::<Unique>(bytes)
        .map(|u| u.0)
        .map_err(|_| Refused)
}

/// Deliberately supports only a single Content-Length HTTP/1.1 200 response.
/// Chunked, close-delimited, compressed and partial responses refuse. No
/// status, including 404, can substitute for an empty complete inventory.
fn body(response: &[u8], elapsed: Duration) -> Result<&[u8], Refused> {
    if elapsed >= DEADLINE || response.len() > MAX_HEADERS + MAX_BODY {
        return Err(Refused);
    }
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or(Refused)?;
    if split + 4 > MAX_HEADERS {
        return Err(Refused);
    }
    let headers = std::str::from_utf8(&response[..split]).map_err(|_| Refused)?;
    let mut lines = headers.split("\r\n");
    if lines.next() != Some("HTTP/1.1 200 OK") {
        return Err(Refused);
    }
    let mut seen = BTreeSet::new();
    let mut length = None;
    let mut content_type = false;
    for line in lines {
        let (key, value) = line.split_once(':').ok_or(Refused)?;
        if key.is_empty() || !key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
            return Err(Refused);
        }
        let key = key.to_ascii_lowercase();
        if !seen.insert(key.clone()) || value.bytes().any(|c| c < 32 && c != b'\t' || c == 127) {
            return Err(Refused);
        }
        let value = value.trim();
        match key.as_str() {
            "content-length" => {
                if value.is_empty() || !value.bytes().all(|c| c.is_ascii_digit()) {
                    return Err(Refused);
                }
                length = Some(value.parse::<usize>().map_err(|_| Refused)?);
            }
            "content-type" => content_type = value == "application/json",
            "transfer-encoding" | "content-encoding" | "content-range" | "trailer" => {
                return Err(Refused);
            }
            _ => {}
        }
    }
    let payload = &response[split + 4..];
    if !content_type
        || length != Some(payload.len())
        || payload.is_empty()
        || payload.len() > MAX_BODY
    {
        return Err(Refused);
    }
    Ok(payload)
}

fn bounded(v: &Value, depth: usize) -> bool {
    if depth > 32 {
        return false;
    }
    match v {
        Value::String(s) => s.len() <= MAX_STRING,
        Value::Array(a) => a.len() <= MAX_RECORDS && a.iter().all(|v| bounded(v, depth + 1)),
        Value::Object(o) => {
            o.len() <= 256
                && o.iter()
                    .all(|(k, v)| k.len() <= MAX_STRING && bounded(v, depth + 1))
        }
        _ => true,
    }
}

/// Exact identity comparisons are ORed, never sent as Engine label filters.
/// Labels use the reviewed caller's fixed keys, not a runtime query language.
pub struct Identity<'a> {
    pub name: &'a str,
    pub labels: &'a [(&'a str, &'a str)],
}

#[derive(Debug, PartialEq, Eq)]
pub struct Inventory {
    pub records: usize,
    pub conflicting_ids: Vec<String>,
}

pub fn inventory(
    response: &[u8],
    elapsed: Duration,
    identity: Identity<'_>,
) -> Result<Inventory, Refused> {
    if identity.name.is_empty()
        || identity.name.len() > MAX_STRING
        || identity.labels.is_empty()
        || identity.labels.len() > 256
        || identity.labels.iter().any(|(k, v)| {
            k.is_empty() || v.is_empty() || k.len() > MAX_STRING || v.len() > MAX_STRING
        })
    {
        return Err(Refused);
    }
    let value = json(body(response, elapsed)?)?;
    if !bounded(&value, 0) {
        return Err(Refused);
    }
    let records = value.as_array().ok_or(Refused)?;
    let mut ids = BTreeSet::new();
    let mut conflicts = Vec::new();
    for record in records {
        let record = record.as_object().ok_or(Refused)?;
        let id = record.get("Id").and_then(Value::as_str).ok_or(Refused)?;
        if id.len() != 64
            || !id
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            || !ids.insert(id)
        {
            return Err(Refused);
        }
        let state = record.get("State").and_then(Value::as_str).ok_or(Refused)?;
        if ![
            "created",
            "restarting",
            "running",
            "removing",
            "paused",
            "exited",
            "dead",
        ]
        .contains(&state)
        {
            return Err(Refused);
        }
        let names = record
            .get("Names")
            .and_then(Value::as_array)
            .ok_or(Refused)?;
        if names.is_empty() {
            return Err(Refused);
        }
        let mut unique_names = BTreeSet::new();
        let mut conflict = false;
        for name in names {
            let name = name.as_str().ok_or(Refused)?;
            let bare = name.strip_prefix('/').ok_or(Refused)?;
            if bare.is_empty()
                || !bare
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
                || !unique_names.insert(name)
            {
                return Err(Refused);
            }
            conflict |= bare == identity.name;
        }
        let labels = record
            .get("Labels")
            .and_then(Value::as_object)
            .ok_or(Refused)?;
        for (key, value) in labels {
            let value = value.as_str().ok_or(Refused)?;
            conflict |= identity.labels.iter().any(|(k, v)| key == k && value == *v);
        }
        if conflict {
            conflicts.push(id.to_owned());
        }
    }
    Ok(Inventory {
        records: records.len(),
        conflicting_ids: conflicts,
    })
}

/// Exact retained bytes, NOT an approved seccomp profile. JSON syntax checking
/// here supplies no architecture/rule validation or loaded-filter evidence.
pub struct PolicyBytes(String);

impl PolicyBytes {
    pub fn retain(bytes: &[u8]) -> Result<Self, Refused> {
        if bytes.is_empty() || bytes.len() > MAX_POLICY || !json(bytes)?.is_object() {
            return Err(Refused);
        }
        Ok(Self(
            std::str::from_utf8(bytes).map_err(|_| Refused)?.to_owned(),
        ))
    }

    pub fn sha256(&self) -> [u8; 32] {
        Sha256::digest(self.0.as_bytes()).into()
    }

    /// Closed policy fragment, deliberately not a runnable create specification.
    pub fn encode_fragment(&self) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"HostConfig": {"SecurityOpt": ["no-new-privileges=true", format!("seccomp={}", self.0)]}})).expect("strings encode")
    }

    /// Compare a decoded inspect SecurityOpt array only. Full inspect/runtime
    /// validation remains unavailable. No failed inspect may call this a pass.
    pub fn inspect_parity(&self, security_opt_json: &[u8]) -> Result<(), Refused> {
        if security_opt_json.len() > MAX_POLICY * 6 + 1024 {
            return Err(Refused);
        }
        let actual = json(security_opt_json)?;
        let expected = serde_json::json!(["no-new-privileges=true", format!("seccomp={}", self.0)]);
        if actual != expected {
            return Err(Refused);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
