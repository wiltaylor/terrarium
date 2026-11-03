//! The `webhook` action of a schedule or a trigger (docs/api.md §19.5):
//! the shape of the action, and the pure part of a delivery. The daemon
//! (`terrariumd::webhooks`) checks the URL, opens the secrets, signs the
//! body, and sends the request.
//!
//! A delivery sends one document about one event:
//!
//! ```json
//! {"event": {"seq": 7, "id": "...", "kind": "monitor.status", "at": "...",
//!            "project": "terra", "agent": null, "actor": "system",
//!            "summary": "...", "data": {...}},
//!  "trigger": {"name": "disk-alert", "project": "terra"}}
//! ```
//!
//! [`render`] makes the body from it: the document as JSON (`json`), a
//! chat preset (`slack`, `discord`), an ntfy message (`ntfy`), or a body
//! template (`template`) with `{{event.kind}}`, `{{event.data.status}}`,
//! `{{trigger.name}}` placeholders ([`render_template`]).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The default request time limit, in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 10;
/// The longest request time limit, in seconds.
pub const MAX_TIMEOUT_SECS: u64 = 30;
/// The largest request body, in bytes.
pub const MAX_BODY: usize = 256 * 1024;
/// The longest body template, in bytes.
pub const MAX_TEMPLATE: usize = 64 * 1024;
/// The most headers of one webhook.
pub const MAX_HEADERS: usize = 20;
/// The default signature header.
pub const SIGNATURE_HEADER: &str = "X-Terrarium-Signature";
/// The header with the time of the delivery (Unix seconds).
pub const TIMESTAMP_HEADER: &str = "X-Terrarium-Timestamp";
/// The header with the delivery ID.
pub const DELIVERY_HEADER: &str = "X-Terrarium-Delivery";
/// The header with the event kind.
pub const EVENT_HEADER: &str = "X-Terrarium-Event";
/// The default content type.
pub const JSON_TYPE: &str = "application/json";

/// The `webhook` action: send an HTTP request about each event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookAction {
    /// An `http` or `https` URL with no user or password. Empty when
    /// `url_secret` gives the URL.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// The name of a secret whose value is the URL (a Slack or Discord
    /// webhook URL is a credential). The project's secret first, then the
    /// global one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_secret: Option<String>,
    #[serde(default)]
    pub method: WebhookMethod,
    #[serde(default)]
    pub format: WebhookFormat,
    /// The body template of `format = template`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// The `Content-Type`. Default: `application/json`; `text/plain` for
    /// `ntfy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<WebhookHeader>,
    /// Sign the body with HMAC-SHA256.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing: Option<WebhookSigning>,
    /// The request time limit: default 10, at most 30.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// The HTTP method of a webhook.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum WebhookMethod {
    #[default]
    #[serde(alias = "post")]
    Post,
    #[serde(alias = "put")]
    Put,
}

impl WebhookMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Post => "POST",
            Self::Put => "PUT",
        }
    }
}

/// The body of a webhook.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebhookFormat {
    /// The event document as JSON.
    #[default]
    Json,
    /// `{"text": "<kind>: <summary>"}`.
    Slack,
    /// `{"content": "<kind>: <summary>"}`.
    Discord,
    /// The summary as plain text, with `Title`, `Priority`, and `Tags`.
    Ntfy,
    /// The body template.
    Template,
}

impl WebhookFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Slack => "slack",
            Self::Discord => "discord",
            Self::Ntfy => "ntfy",
            Self::Template => "template",
        }
    }
}

/// One request header: a fixed `value`, or the value of a `secret`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookHeader {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// The name of a secret: the header value is its main field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

/// The signature of a webhook body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookSigning {
    /// The name of the secret that holds the HMAC key.
    pub secret: String,
    #[serde(default = "signature_header")]
    pub header: String,
    #[serde(default)]
    pub algo: SigningAlgo,
}

/// The HMAC hash. Only SHA-256.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SigningAlgo {
    #[default]
    Sha256,
}

fn signature_header() -> String {
    SIGNATURE_HEADER.into()
}

impl WebhookAction {
    /// The time limit of one request, in seconds.
    pub fn timeout(&self) -> u64 {
        self.timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS)
    }

    /// The `Content-Type` of the body.
    pub fn content_type(&self) -> &str {
        match (&self.content_type, self.format) {
            (Some(c), _) if !c.trim().is_empty() => c.trim(),
            (_, WebhookFormat::Ntfy) => "text/plain; charset=utf-8",
            _ => JSON_TYPE,
        }
    }

    /// The names of the secrets that the action uses: the URL, the
    /// headers, and the signing key.
    pub fn secrets(&self) -> Vec<(&'static str, &str)> {
        let mut v = Vec::new();
        if let Some(s) = &self.url_secret {
            v.push(("action.url_secret", s.as_str()));
        }
        for h in &self.headers {
            if let Some(s) = &h.secret {
                v.push(("action.headers", s.as_str()));
            }
        }
        if let Some(s) = &self.signing {
            v.push(("action.signing.secret", s.secret.as_str()));
        }
        v
    }

    /// Check the fields that need no store and no network. The error is
    /// `(field, message)`. The daemon checks the URL itself
    /// (`terrariumd::webhooks::net`).
    pub fn check(&self) -> Result<(), (&'static str, String)> {
        match (self.url.trim().is_empty(), &self.url_secret) {
            (true, None) => return Err(("action.url", "give a URL or a url_secret".into())),
            (false, Some(_)) => {
                return Err(("action.url", "give a URL or a url_secret, not both".into()));
            }
            (true, Some(s)) if s.trim().is_empty() => {
                return Err(("action.url_secret", "name a secret".into()));
            }
            _ => {}
        }
        if let Some(t) = self.timeout_secs
            && (t == 0 || t > MAX_TIMEOUT_SECS)
        {
            return Err((
                "action.timeout_secs",
                format!("{t} is not 1-{MAX_TIMEOUT_SECS}"),
            ));
        }
        match (self.format, &self.template) {
            (WebhookFormat::Template, None) => {
                return Err((
                    "action.template",
                    "format `template` needs a template".into(),
                ));
            }
            (WebhookFormat::Template, Some(t)) => {
                if t.len() > MAX_TEMPLATE {
                    return Err((
                        "action.template",
                        format!("the template is over {MAX_TEMPLATE} bytes"),
                    ));
                }
                check_template(t).map_err(|e| ("action.template", e))?;
            }
            (_, Some(_)) => {
                return Err((
                    "action.template",
                    "a template needs format `template`".into(),
                ));
            }
            (_, None) => {}
        }
        if let Some(c) = &self.content_type
            && (c.trim().is_empty() || c.chars().any(|ch| ch.is_control()))
        {
            return Err(("action.content_type", format!("bad content type `{c}`")));
        }
        if self.headers.len() > MAX_HEADERS {
            return Err(("action.headers", format!("at most {MAX_HEADERS} headers")));
        }
        for h in &self.headers {
            check_header_name(&h.name).map_err(|e| ("action.headers", e))?;
            match (&h.value, &h.secret) {
                (Some(v), None) => {
                    if v.chars().any(|c| c.is_control()) {
                        return Err((
                            "action.headers",
                            format!("header `{}`: a value has no control characters", h.name),
                        ));
                    }
                }
                (None, Some(s)) if !s.trim().is_empty() => {}
                _ => {
                    return Err((
                        "action.headers",
                        format!("header `{}`: give a value or a secret, not both", h.name),
                    ));
                }
            }
        }
        if let Some(s) = &self.signing {
            if s.secret.trim().is_empty() {
                return Err(("action.signing.secret", "name a secret".into()));
            }
            check_header_name(&s.header).map_err(|e| ("action.signing.header", e))?;
        }
        Ok(())
    }
}

/// The headers that the daemon sets itself, or that HTTP owns. A webhook
/// cannot set them.
const RESERVED: &[&str] = &[
    "host",
    "content-length",
    "content-type",
    "transfer-encoding",
    "connection",
    "upgrade",
    "te",
    "trailer",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
];

/// Check a header name: a token of RFC 9110, not a reserved header, not an
/// `X-Terrarium-*` header (the daemon's own).
pub fn check_header_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c));
    if !ok {
        return Err(format!("bad header name `{name}`"));
    }
    let lower = name.to_ascii_lowercase();
    if RESERVED.contains(&lower.as_str()) {
        return Err(format!(
            "header `{name}` is set by the daemon or by HTTP (use content_type for Content-Type)"
        ));
    }
    if lower.starts_with("x-terrarium-") && lower != SIGNATURE_HEADER.to_ascii_lowercase() {
        return Err(format!(
            "header `{name}`: X-Terrarium-* headers are the daemon's"
        ));
    }
    Ok(())
}

// ----------------------------------------------------------- the document

/// The event of a delivery: the fields of the document.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventDoc {
    pub seq: i64,
    pub id: String,
    pub kind: String,
    pub at: String,
    pub project: Option<String>,
    pub agent: Option<String>,
    pub actor: Option<String>,
    pub summary: String,
    pub data: Value,
}

/// The document of a delivery: `{event, trigger}`.
pub fn document(e: &EventDoc, trigger: &str, project: Option<&str>) -> Value {
    json!({
        "event": e,
        "trigger": {"name": trigger, "project": project},
    })
}

/// A short line about the event for a chat message: `<kind>: <summary>`
/// (the summary of the event log, which the trigger mail shows too). A
/// project event starts with `[project]`.
pub fn summary_line(e: &EventDoc) -> String {
    let what = if e.summary.trim().is_empty() {
        e.kind.clone()
    } else {
        format!("{}: {}", e.kind, e.summary.trim())
    };
    match &e.project {
        Some(p) => format!("[{p}] {what}"),
        None => what,
    }
}

/// The ntfy priority (1-5) of an event: a monitor status by its new
/// status, a CI failure 4, any other event 3.
pub fn ntfy_priority(e: &EventDoc) -> u8 {
    let status = e.data["to"]
        .as_str()
        .or_else(|| e.data["status"].as_str())
        .unwrap_or_default();
    if e.kind.starts_with("monitor.") {
        return match status {
            "crit" => 5,
            "warn" | "unknown" => 4,
            "ok" => 2,
            _ => 3,
        };
    }
    if e.kind.ends_with(".failed") || status == "failed" || status == "giving_up" {
        return 4;
    }
    3
}

/// The ntfy tags (emoji short codes) of an event.
pub fn ntfy_tags(e: &EventDoc) -> String {
    let status = e.data["to"]
        .as_str()
        .or_else(|| e.data["status"].as_str())
        .unwrap_or_default();
    let tag = match status {
        "crit" => "rotating_light",
        "warn" | "unknown" => "warning",
        "ok" => "white_check_mark",
        "failed" | "giving_up" => "x",
        _ if e.kind.ends_with(".failed") => "x",
        _ => "bell",
    };
    format!("{tag},{}", e.kind.replace(',', "_"))
}

/// The rendered body, and the headers that the format adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub body: String,
    pub headers: Vec<(String, String)>,
}

/// One line of header text: no control characters, at most 250
/// characters.
fn header_text(s: &str) -> String {
    let line: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(250)
        .collect();
    line.trim().to_owned()
}

/// Make the body of a delivery. A template that does not parse renders as
/// it is (the API checks templates at write time).
pub fn render(a: &WebhookAction, e: &EventDoc, trigger: &str, project: Option<&str>) -> Rendered {
    let doc = document(e, trigger, project);
    let line = summary_line(e);
    match a.format {
        WebhookFormat::Json => Rendered {
            body: doc.to_string(),
            headers: Vec::new(),
        },
        WebhookFormat::Slack => Rendered {
            body: json!({"text": line}).to_string(),
            headers: Vec::new(),
        },
        WebhookFormat::Discord => {
            // Discord takes at most 2000 characters.
            let content: String = line.chars().take(2000).collect();
            Rendered {
                body: json!({"content": content}).to_string(),
                headers: Vec::new(),
            }
        }
        WebhookFormat::Ntfy => {
            let title = match &e.project {
                Some(p) => format!("{p}: {}", e.kind),
                None => e.kind.clone(),
            };
            let body = if e.summary.trim().is_empty() {
                e.kind.clone()
            } else {
                e.summary.trim().to_owned()
            };
            Rendered {
                body,
                headers: vec![
                    ("Title".into(), header_text(&title)),
                    ("Priority".into(), ntfy_priority(e).to_string()),
                    ("Tags".into(), header_text(&ntfy_tags(e))),
                ],
            }
        }
        WebhookFormat::Template => {
            let t = a.template.as_deref().unwrap_or_default();
            let json_mode = is_json_type(a.content_type());
            Rendered {
                body: render_template(t, &doc, json_mode),
                headers: Vec::new(),
            }
        }
    }
}

/// Is the content type JSON (`application/json`, `application/*+json`)?
pub fn is_json_type(ct: &str) -> bool {
    let base = ct
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    base == "application/json" || base.ends_with("+json")
}

// ---------------------------------------------------------- the templates

/// One piece of a body template.
enum Piece<'a> {
    Text(&'a str),
    /// A path, and `| json`.
    Field(&'a str, bool),
}

fn known_root(path: &str) -> bool {
    let mut segs = path.split('.');
    let root = segs.next().unwrap_or_default();
    matches!(root, "event" | "trigger") && path.split('.').all(|s| !s.is_empty())
}

fn pieces(template: &str) -> Result<Vec<Piece<'_>>, String> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        if open > 0 {
            out.push(Piece::Text(&rest[..open]));
        }
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            return Err("`{{` without `}}`: write `{{event.kind}}`".into());
        };
        let inner = after[..close].trim();
        let (path, raw) = match inner.split_once('|') {
            Some((p, f)) if f.trim() == "json" => (p.trim(), true),
            Some((_, f)) => {
                return Err(format!(
                    "unknown filter `{}` in `{{{{{inner}}}}}`: the only filter is `json`",
                    f.trim()
                ));
            }
            None => (inner, false),
        };
        if !known_root(path) {
            return Err(format!(
                "unknown placeholder `{{{{{inner}}}}}`: use event.kind, event.summary, \
                 event.project, event.at, event.data.<path>, or trigger.name"
            ));
        }
        out.push(Piece::Field(path, raw));
        rest = &after[close + 2..];
    }
    if !rest.is_empty() {
        out.push(Piece::Text(rest));
    }
    Ok(out)
}

/// Check a body template: each `{{...}}` names `event.<path>` or
/// `trigger.<path>`, with an optional `| json`.
pub fn check_template(template: &str) -> Result<(), String> {
    pieces(template).map(|_| ())
}

/// The value at a dotted path of `doc`. A number segment indexes an array.
pub fn lookup<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    let mut at = doc;
    for seg in path.split('.') {
        at = match at {
            Value::Object(m) => m.get(seg)?,
            Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(at)
}

/// A string escaped for the inside of a JSON string (no quotes).
pub fn json_escape(s: &str) -> String {
    let quoted = Value::String(s.to_owned()).to_string();
    quoted[1..quoted.len() - 1].to_owned()
}

/// Fill a body template from the document. A missing value is empty.
///
/// - `{{path}}`: a string as it is, `null` as empty, any other value as
///   compact JSON. With `json_mode` (a JSON content type), the text is
///   escaped for the inside of a JSON string, so
///   `"{{event.summary}}"` stays valid JSON.
/// - `{{path | json}}`: the value as a JSON literal (a string with its
///   quotes; missing is `null`), for a place outside a JSON string.
pub fn render_template(template: &str, doc: &Value, json_mode: bool) -> String {
    let Ok(parts) = pieces(template) else {
        return template.to_owned();
    };
    let mut out = String::with_capacity(template.len());
    for p in parts {
        match p {
            Piece::Text(t) => out.push_str(t),
            Piece::Field(path, true) => {
                out.push_str(&lookup(doc, path).unwrap_or(&Value::Null).to_string());
            }
            Piece::Field(path, false) => {
                let text = match lookup(doc, path) {
                    None | Some(Value::Null) => String::new(),
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                };
                if json_mode {
                    out.push_str(&json_escape(&text));
                } else {
                    out.push_str(&text);
                }
            }
        }
    }
    out
}

// ----------------------------------------------------------- the samples

/// A sample event of `kind` for a test send or a preview. A known kind
/// has data of its real shape; a glob (`ci.*`) becomes a kind that it
/// matches.
pub fn sample_event(kind: &str, project: Option<&str>, at: &str) -> EventDoc {
    let kind = if kind.contains('*') || kind.contains('?') {
        let k = kind.replace('?', "x");
        match k.as_str() {
            "*" | "monitor.*" => "monitor.status".to_owned(),
            "pipeline.*" => "pipeline.status".to_owned(),
            other => other.replace('*', "sample"),
        }
    } else {
        kind.to_owned()
    };
    let p = project.unwrap_or("demo");
    let (summary, data) = match kind.as_str() {
        "monitor.status" => (
            "monitor disk-usage: ok → crit 93 %".to_owned(),
            json!({"project": p, "monitor": "disk-usage", "monitor_id": "01SAMPLE",
                   "from": "ok", "to": "crit", "value": 93, "unit": "%",
                   "message": "/ is 93% full", "error": null}),
        ),
        "monitor.result" => (
            "monitor disk-usage: crit 93 %".to_owned(),
            json!({"project": p, "monitor": "disk-usage", "monitor_id": "01SAMPLE",
                   "status": "crit", "value": 93, "unit": "%", "message": "/ is 93% full",
                   "error": null, "duration_ms": 120}),
        ),
        "pipeline.status" => (
            "pipeline ci: success → failed (CI #412)".to_owned(),
            json!({"project": p, "pipeline": "ci", "pipeline_id": "01SAMPLE",
                   "from": "success", "to": "failed", "run_id": "9123456789",
                   "run_name": "CI #412", "url": "https://github.com/o/r/actions/runs/9123456789",
                   "branch": "main"}),
        ),
        "pipeline.run" => (
            "pipeline deploys: 1.4.2 to Production running → failed".to_owned(),
            json!({"project": p, "pipeline": "deploys", "pipeline_id": "01SAMPLE",
                   "run_id": "Deployments-123", "name": "1.4.2 to Production",
                   "from": "running", "to": "failed",
                   "url": "https://octopus.example/app#/deployments/Deployments-123",
                   "branch": null, "commit": null, "environment": "Production",
                   "summary": "Step 2 failed"}),
        ),
        "pipeline.error" => (
            "pipeline ci: the poll failed: GitHub refused the token (401)".to_owned(),
            json!({"project": p, "pipeline": "ci", "pipeline_id": "01SAMPLE",
                   "error": "GitHub refused the token (401)"}),
        ),
        k if k.starts_with("ci.") => (
            format!("{k} on main"),
            json!({"repo": "wil/terrarium", "branch": "main", "run_id": 42,
                   "run_url": "https://ci.example/run/42", "conclusion": "failure"}),
        ),
        k => (format!("a sample {k} event"), json!({"sample": true})),
    };
    EventDoc {
        seq: 0,
        id: "01SAMPLE0000000000000000000".into(),
        kind,
        at: at.to_owned(),
        project: project.map(str::to_owned),
        agent: None,
        actor: Some("system".into()),
        summary,
        data,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn ev() -> EventDoc {
        sample_event("monitor.status", Some("terra"), "2026-11-03T00:00:00.000Z")
    }

    fn action(format: WebhookFormat) -> WebhookAction {
        WebhookAction {
            url: "https://example.test/hook".into(),
            url_secret: None,
            method: WebhookMethod::Post,
            format,
            template: None,
            content_type: None,
            headers: Vec::new(),
            signing: None,
            timeout_secs: None,
        }
    }

    #[test]
    fn the_formats() {
        let e = ev();
        let j = render(&action(WebhookFormat::Json), &e, "disk", Some("terra"));
        let v: Value = serde_json::from_str(&j.body).unwrap();
        assert_eq!(v["event"]["kind"], "monitor.status");
        assert_eq!(v["event"]["data"]["to"], "crit");
        assert_eq!(v["trigger"]["name"], "disk");
        assert_eq!(v["trigger"]["project"], "terra");
        let s = render(&action(WebhookFormat::Slack), &e, "disk", None);
        let v: Value = serde_json::from_str(&s.body).unwrap();
        assert_eq!(
            v["text"],
            "[terra] monitor.status: monitor disk-usage: ok → crit 93 %"
        );
        let d = render(&action(WebhookFormat::Discord), &e, "disk", None);
        let v: Value = serde_json::from_str(&d.body).unwrap();
        assert!(v["content"].as_str().unwrap().contains("ok → crit"));
        let n = render(&action(WebhookFormat::Ntfy), &e, "disk", None);
        assert_eq!(n.body, "monitor disk-usage: ok → crit 93 %");
        assert!(n.headers.contains(&("Priority".into(), "5".into())));
        assert!(
            n.headers
                .contains(&("Title".into(), "terra: monitor.status".into()))
        );
        assert!(
            n.headers
                .iter()
                .any(|(k, v)| k == "Tags" && v.starts_with("rotating_light"))
        );
        assert_eq!(
            action(WebhookFormat::Ntfy).content_type(),
            "text/plain; charset=utf-8"
        );
        let mut warn = ev();
        warn.data["to"] = json!("warn");
        assert_eq!(ntfy_priority(&warn), 4);
        warn.data["to"] = json!("ok");
        assert_eq!(ntfy_priority(&warn), 2);
        let ci = sample_event("ci.run.failed", None, "t");
        assert_eq!(ntfy_priority(&ci), 4);
        assert_eq!(sample_event("ci.*", None, "t").kind, "ci.sample");
        assert_eq!(sample_event("*", None, "t").kind, "monitor.status");
        assert_eq!(
            sample_event("pipeline.*", None, "t").kind,
            "pipeline.status"
        );
        assert_eq!(sample_event("pipeline.run", None, "t").data["to"], "failed");
    }

    #[test]
    fn templates_escape_and_leave_missing_values_empty() {
        let mut e = ev();
        e.summary = "a \"quoted\"\nline\\".into();
        let doc = document(&e, "disk", Some("terra"));
        let t = r#"{"s": "{{event.summary}}", "to": "{{event.data.to}}", "v": {{event.data.value}}, "x": "{{event.data.nope}}", "d": {{event.data | json}}, "m": {{event.data.missing | json}}, "t": "{{trigger.name}}"}"#;
        let out = render_template(t, &doc, true);
        let v: Value = serde_json::from_str(&out).expect(&out);
        assert_eq!(v["s"], "a \"quoted\"\nline\\");
        assert_eq!(v["to"], "crit");
        assert_eq!(v["v"], 93);
        assert_eq!(v["x"], "");
        assert_eq!(v["d"]["monitor"], "disk-usage");
        assert!(v["m"].is_null());
        assert_eq!(v["t"], "disk");
        // Plain text keeps the value as it is.
        let plain = render_template("{{event.kind}} {{event.summary}}", &doc, false);
        assert_eq!(plain, "monitor.status a \"quoted\"\nline\\");
        // A bad placeholder is an error at check time.
        assert!(check_template("{{event.kind}}").is_ok());
        assert!(check_template("{{trigger.name}}").is_ok());
        assert!(check_template("{{nope.kind}}").is_err());
        assert!(check_template("{{event.kind | upper}}").is_err());
        assert!(check_template("{{event.kind").is_err());
        assert!(check_template("{{event..x}}").is_err());
        assert!(is_json_type("application/json; charset=utf-8"));
        assert!(is_json_type("application/vnd.x+json"));
        assert!(!is_json_type("text/plain"));
    }

    #[test]
    fn the_action_shape_and_checks() {
        let a: WebhookAction = serde_json::from_value(json!({
            "url": "https://example.test/x", "method": "put", "format": "template",
            "template": "{\"k\": \"{{event.kind}}\"}",
            "headers": [{"name": "X-Api-Key", "secret": "hook-key"}, {"name": "X-Env", "value": "prod"}],
            "signing": {"secret": "hook-sign"}, "timeout_secs": 5
        }))
        .unwrap();
        assert_eq!(a.method, WebhookMethod::Put);
        assert_eq!(a.signing.as_ref().unwrap().header, SIGNATURE_HEADER);
        assert!(a.check().is_ok());
        assert_eq!(a.timeout(), 5);
        assert_eq!(
            a.secrets(),
            vec![
                ("action.headers", "hook-key"),
                ("action.signing.secret", "hook-sign")
            ]
        );
        let back: WebhookAction =
            serde_json::from_value(serde_json::to_value(&a).unwrap()).unwrap();
        assert_eq!(back, a);
        assert!(serde_json::from_value::<WebhookAction>(json!({"url": "x", "nope": 1})).is_err());
        let bad = |f: &dyn Fn(&mut WebhookAction)| {
            let mut b = a.clone();
            f(&mut b);
            b.check().unwrap_err().0
        };
        assert_eq!(bad(&|b| b.url.clear()), "action.url");
        assert_eq!(bad(&|b| b.url_secret = Some("u".into())), "action.url");
        assert_eq!(bad(&|b| b.timeout_secs = Some(31)), "action.timeout_secs");
        assert_eq!(bad(&|b| b.template = None), "action.template");
        assert_eq!(bad(&|b| b.format = WebhookFormat::Json), "action.template");
        assert_eq!(
            bad(&|b| b.template = Some("{{bad}}".into())),
            "action.template"
        );
        assert_eq!(
            bad(&|b| b.headers[0].name = "Content-Length".into()),
            "action.headers"
        );
        assert_eq!(
            bad(&|b| b.headers[0].name = "X-Terrarium-Delivery".into()),
            "action.headers"
        );
        assert_eq!(
            bad(&|b| b.headers[0].name = "bad name".into()),
            "action.headers"
        );
        assert_eq!(
            bad(&|b| b.headers[1].value = Some("a\r\nInjected: 1".into())),
            "action.headers"
        );
        assert_eq!(
            bad(&|b| b.headers[1].secret = Some("s".into())),
            "action.headers"
        );
        assert_eq!(
            bad(&|b| b.signing.as_mut().unwrap().secret.clear()),
            "action.signing.secret"
        );
    }
}
