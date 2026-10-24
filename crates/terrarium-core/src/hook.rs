use serde::{Deserialize, Serialize};

use crate::harness::HarnessId;

/// A hook call from a harness, as forwarded by `terrarium hook <event>` to the
/// runner socket. The body is the harness's own JSON, kept generic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookPayload {
    pub harness: HarnessId,
    /// The hook name as the harness calls it, for example `Stop` or
    /// `UserPromptSubmit`.
    pub hook: String,
    #[serde(default)]
    pub body: serde_json::Value,
}

impl HookPayload {
    /// Read a string field from the body, if it is there.
    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.body.get(key).and_then(|v| v.as_str())
    }
}
