//! Per-agent files for pi (PLAN §8 and §11).
//!
//! All agents share one pi home (`/auth/pi`, seen as `PI_CODING_AGENT_DIR`)
//! for `auth.json` (no login: pi uses API keys only) and the custom
//! providers (`models.json`).
//! Everything that is per agent lives in the harness directory
//! (`/state/agents/<ulid>/harness/`). The sandbox binds that directory
//! read-only at the same path, so these paths are the same for the daemon
//! and for pi. The two places that pi writes, `pi/sessions/` and the
//! settings overlay, get read-write binds of their own.
//!
//! | File | Use |
//! |---|---|
//! | `prompt.md` | the rendered system prompt (the daemon writes it); `launch` passes its path |
//! | `pi/terrarium.ts` | the event extension ([`EXTENSION`]); `launch` loads it with `-e` |
//! | `pi/sessions/` | the session files (`--session-dir`); bound read-write over itself |
//! | `pi/settings.json` | bound over `$PI_CODING_AGENT_DIR/settings.json` |
//! | `pi/models.json` | bound read-only over `$PI_CODING_AGENT_DIR/models.json` |
//!
//! # What pi keeps in its home, and where it goes
//!
//! | Path in the home | Decision | Why |
//! |---|---|---|
//! | `auth.json` | shared, `{}` | no login: pi uses API keys only (PLAN §16.3), and each agent gets its key in the env. An install from before had the pi plan accounts and made the file a symlink to `/run/terrarium/cred/account/auth.json`; the daemon makes it a plain `{}` file again (`terrariumd::accounts`). |
//! | `models.json` | per agent, from the shared file | custom providers (base URL, models, API key). Read only for pi. Every start copies the shared file and adds the Terrarium providers ([`TERRARIUM_PROVIDERS`]: `zai-api`, the Z.ai pay-as-you-go endpoint, and `kimi-coding-intl`, the overseas Kimi Code endpoint) and adds `k3.1-pro` and `k3.1-pro-highspeed` to the models of the built-in `kimi-coding`. An edit of the shared file reaches an agent at its next start. |
//! | `settings.json` | per agent | the operator's own defaults (provider, model, theme, packages) stay out, and a `/model` change in one agent does not change the default of the others. The first start copies the shared file; later starts keep the agent's copy. Every start sets `lastChangelogVersion` to the pinned version ([`LAST_CHANGELOG_KEY`]), so pi 1.4's "What's new" changelog does not show. |
//! | `mcp.json` | not loaded | `--no-mcp` (pi 1.4 has built-in MCP). |
//! | `sessions/` | not used | `--session-dir` puts the sessions in the agent directory. In the sandbox every agent has the cwd `$HOME/work`, so the default directory (`sessions/--home-…-work--/`) would be one directory for all agents, and `pi --resume` would list the sessions of every agent. Session IDs do not collide (each agent gets new UUIDs), but pi has **no lock on a session**: two processes with one `--session-id` both start and both append (`docs/compat.md`). The supervisor must run one pi per agent. |
//! | `extensions/`, `skills/`, `prompts/` | not loaded | `--no-extensions`, `--no-skills`, `--no-prompt-templates`. |
//! | `AGENTS.md` | not loaded | `--no-context-files`. The operator's personal instructions stay out of agents. |
//! | `SYSTEM.md`, `APPEND_SYSTEM.md` | per agent, empty | pi reads both from the home whenever the matching flag is absent (`resource-loader.js` `discoverSystemPromptFile`, `discoverAppendSystemPromptFile`), and `--no-context-files` does not stop it. Each prompt flag replaces only its own file: in append mode pi would take the operator's `SYSTEM.md` instead of its default prompt, and in replace mode it would append the operator's `APPEND_SYSTEM.md` (B1). Two fixes, each enough alone: empty per-agent files bound read-only over both (an empty file gives no prompt text), and the other prompt flag with an empty value (an empty value stops the discovery; [`crate::launch`]). |
//! | `trust.json` | not written | `--no-approve` makes no trust decision. |
//! | `models-store.json` | shared | written at start (seen in the live check). Not tested with many writers. |
//!
//! The extension file is also bound read-only over itself. The harness
//! directory is read-only already; the explicit bind keeps the extension
//! safe if the bind of the directory ever changes, because the agent must
//! not change the code that reports its state.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use terrarium_core::{Agent, FileOverlay};

use crate::PiOptions;
use crate::error::{PiError, Result};

/// The Terrarium event extension (`ext/terrarium.ts`).
pub const EXTENSION: &str = include_str!("../ext/terrarium.ts");
/// The file name of the extension in the agent's pi directory.
pub const EXTENSION_FILE: &str = "terrarium.ts";
/// The settings file in the pi home.
pub const SETTINGS_FILE: &str = "settings.json";
/// The settings key of the last version whose changelog pi showed. pi
/// shows the "What's new" entries of every newer version at the start,
/// above the editor. The per-agent copy always holds the pinned version,
/// so the changelog never shows (the shared file can name an older one).
pub const LAST_CHANGELOG_KEY: &str = "lastChangelogVersion";
/// The custom providers in the pi home. The agent gets a copy with the
/// Terrarium providers added.
pub const MODELS_FILE: &str = "models.json";
/// The providers that Terrarium adds to every agent's `models.json`. A
/// provider of the same id in the shared file is replaced.
pub const TERRARIUM_PROVIDERS: &[&str] = &[
    terrarium_core::zai::PI_API_PROVIDER,
    terrarium_core::moonshot::PI_CODING_PROVIDER,
];
/// The logins in the pi home. Shared.
pub const AUTH_FILE: &str = "auth.json";
/// The session directory in the agent's pi directory.
pub const SESSIONS_DIR: &str = "sessions";
/// The rendered prompt in the agent directory (PLAN §11).
pub const PROMPT_FILE: &str = "prompt.md";
/// The prompt files that pi discovers in its home (B1). Each agent gets
/// empty ones over them.
pub const DISCOVERED_PROMPT_FILES: &[&str] = &["SYSTEM.md", "APPEND_SYSTEM.md"];
/// The settings keys that Terrarium forces in the agent's copy at each
/// start, over the user file (`docs/research/harness-hardening.md`, pi,
/// 2), beside [`LAST_CHANGELOG_KEY`]:
///
/// - `doubleEscapeAction: "none"`: two Esc keys on an empty editor open
///   the tree selector, which waits for a key. Terrarium sends Esc for a
///   deny and a leader can send it (`agent keys`).
/// - `branchSummary.skipPrompt: true`: no "Summarize branch?" dialog.
/// - `defaultProjectTrust: "never"`: no trust dialog, beside `--no-approve`.
/// - `tuiMode: "regular"`: the screen the detector knows, beside the flag.
pub fn forced_settings() -> Value {
    json!({
        "doubleEscapeAction": "none",
        "branchSummary": {"skipPrompt": true},
        "defaultProjectTrust": "never",
        "tuiMode": "regular",
    })
}

/// `/state/agents/<ulid>/harness`: the directory of the per-agent harness
/// files, next to the workspace. The sandbox binds it read-only at the
/// same path (`docs/runner.md`, "Files"); only the overlays that the
/// harness writes are read-write. The runner's own records are in the
/// sibling `private/`, which the sandbox never binds.
pub fn agent_dir(agent: &Agent) -> PathBuf {
    if let Some(d) = &agent.harness_dir {
        return d.clone();
    }
    agent
        .workspace
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| agent.workspace.clone())
        .join(HARNESS_DIR)
}

/// The name of the harness directory in the agent directory
/// (`terrarium_runner::HARNESS_DIR`).
pub const HARNESS_DIR: &str = "harness";

/// `/state/agents/<ulid>/pi`.
pub fn pi_dir(agent: &Agent) -> PathBuf {
    agent_dir(agent).join("pi")
}

/// The extension file of the agent.
pub fn extension_path(agent: &Agent) -> PathBuf {
    pi_dir(agent).join(EXTENSION_FILE)
}

/// The session directory of the agent (`--session-dir`).
pub fn sessions_dir(agent: &Agent) -> PathBuf {
    pi_dir(agent).join(SESSIONS_DIR)
}

/// The rendered prompt file of the agent.
pub fn prompt_path(agent: &Agent) -> PathBuf {
    agent_dir(agent).join(PROMPT_FILE)
}

/// The prompt file, when it holds text. `launch` passes the path, and pi
/// reads the file. A path that does not exist would go to pi as the
/// prompt text itself, so a missing or empty file gives `None`.
pub fn prompt_file(agent: &Agent) -> Option<PathBuf> {
    let path = prompt_path(agent);
    let text = fs::read_to_string(&path).ok()?;
    (!text.trim().is_empty()).then_some(path)
}

/// Write `bytes` to `path` through a temporary file and a rename.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp-terrarium");
    fs::write(&tmp, bytes).map_err(|e| PiError::io(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| PiError::io(path, e))
}

fn read_json(path: &Path) -> Result<Option<Value>> {
    match fs::read_to_string(path) {
        // A broken file is not fatal: the per-agent copy starts again.
        Ok(s) => Ok(serde_json::from_str(&s).ok().filter(Value::is_object)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(PiError::io(path, e)),
    }
}

/// One GLM model entry for a custom pi provider. The values are pi
/// 1.4.0's built-in `zai` catalog (`pi-ai/dist/providers/data/zai.json`);
/// the prices agree with docs.z.ai/guides/overview/pricing.
fn glm_model(id: &str, name: &str, cost: [f64; 3]) -> Value {
    json!({
        "id": id,
        "name": name,
        "reasoning": true,
        "input": ["text"],
        "cost": {"input": cost[0], "output": cost[1], "cacheRead": cost[2], "cacheWrite": 0},
        "contextWindow": 1_000_000,
        "maxTokens": 131_072,
    })
}

/// The custom pi provider `zai-api`: the Z.ai general (pay as you go)
/// endpoint, OpenAI-compatible, with the key from `$ZAI_API_KEY` (the
/// daemon injects it for an agent on a Z.ai subscription). pi's built-in
/// `zai` provider is the GLM Coding Plan endpoint and needs no entry.
/// pi turns on its Z.ai compatibility (thinking format, tool stream) from
/// the `api.z.ai` base URL.
pub fn zai_api_provider() -> Value {
    use terrarium_core::zai;
    json!({
        "name": "Z.AI (pay as you go)",
        "baseUrl": zai::API_BASE_URL,
        "api": "openai-completions",
        "apiKey": format!("${}", zai::KEY_ENV),
        "models": [
            glm_model(zai::MODEL_FAST, "GLM-5.3-Flash", [0.15, 0.5, 0.03]),
            glm_model(zai::MODEL_FLAGSHIP, "GLM-5.3", [1.4, 4.4, 0.26]),
        ],
    })
}

/// The `compat` block of pi 1.4.0's `kimi-coding` models that think with a
/// `reasoning_effort` (`pi-ai/dist/providers/data/kimi-coding.json`).
fn kimi_compat() -> Value {
    json!({
        "supportsStrictMode": false,
        "supportsStore": false,
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": true,
        "maxTokensField": "max_tokens",
        "thinkingFormat": "openai",
        "requiresReasoningContentOnAssistantMessages": true,
        "supportsMidConvoSystemMessages": true,
        "supportsMidConvoToolAdditions": true,
    })
}

/// The `compat` block of pi 1.4.0's `kimi-for-coding-highspeed`. The model
/// always thinks and takes no effort, so pi gives it the DeepSeek thinking
/// format.
fn kimi_highspeed_compat() -> Value {
    json!({
        "supportsStrictMode": false,
        "supportsStore": false,
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": false,
        "maxTokensField": "max_tokens",
        "thinkingFormat": "deepseek",
        "supportsMidConvoSystemMessages": true,
    })
}

/// The thinking levels of the Kimi Code models with a `reasoning_effort`:
/// low, high and max (kimi.com/code/docs/en/kimi-code/models.html and pi
/// 1.4.0's `kimi-coding` catalog).
fn kimi_levels() -> Value {
    json!({"off": null, "minimal": null, "low": "low", "medium": null,
           "high": "high", "xhigh": null, "max": "max"})
}

/// One Kimi Code model entry for a pi provider. The values are pi 1.4.0's
/// built-in `kimi-coding` catalog
/// (`pi-ai/dist/providers/data/kimi-coding.json`), compat and image limits
/// included, so a custom provider sends the same requests as the built-in
/// one.
fn kimi_model(
    id: &str,
    name: &str,
    context: u64,
    max_tokens: u64,
    cost: [f64; 3],
    levels: Value,
    compat: Value,
) -> Value {
    json!({
        "id": id,
        "name": name,
        "reasoning": true,
        "input": ["text", "image"],
        "inputLimits": {"images": {"resize": {"maxWidth": 2000, "maxHeight": 2000, "maxBytes": 4_718_592, "jpegQuality": 80}}},
        "cost": {"input": cost[0], "output": cost[1], "cacheRead": cost[2], "cacheWrite": 0},
        "contextWindow": context,
        "maxTokens": max_tokens,
        "thinkingLevelMap": levels,
        "compat": compat,
    })
}

/// The Kimi Code models that pi 1.4.0's `kimi-coding` catalog lacks
/// ([`terrarium_core::moonshot::CODING_MODELS_NOT_IN_PI`]).
///
/// Source: kimi.com/code/docs/en/kimi-code/models.html (read 2026-10-29).
/// The context window is up to 1M tokens (1M for the Pro tier and above).
/// The input is images and video (pi takes text and images). The
/// `reasoning_effort` is low, high or max. Thinking cannot be off (the
/// endpoint refuses such a request with HTTP 400), so `off` is null. The
/// page gives no output limit: the value is that of `k3` in pi's catalog.
/// The membership has no price for each token, so the cost is 0, as for
/// pi's `k3-256k`.
fn kimi_models_not_in_pi() -> Vec<Value> {
    let names = ["Kimi K3.1 Pro", "Kimi K3.1 Pro HighSpeed"];
    terrarium_core::moonshot::CODING_MODELS_NOT_IN_PI
        .into_iter()
        .zip(names)
        .map(|(id, name)| {
            kimi_model(
                id,
                name,
                1_048_576,
                131_072,
                [0.0; 3],
                kimi_levels(),
                kimi_compat(),
            )
        })
        .collect()
}

/// The custom pi provider `kimi-coding-intl`: the overseas Kimi Code
/// endpoint (`api.kimi.ai`), OpenAI-compatible, with the key from
/// `$KIMI_API_KEY` (the daemon injects it for an agent on a Moonshot
/// `coding` subscription). The models are pi 1.4.0's built-in
/// `kimi-coding` catalog (which has the China host `api.kimi.com` only)
/// and the Kimi Code models that the catalog lacks.
pub fn kimi_coding_intl_provider() -> Value {
    use terrarium_core::moonshot;
    let mut models = vec![
        kimi_model(
            moonshot::MODEL_CODING_DEFAULT,
            "kimi-for-coding",
            1_048_576,
            32_768,
            [0.95, 4.0, 0.19],
            kimi_levels(),
            kimi_compat(),
        ),
        kimi_model(
            "k3",
            "Kimi K3",
            1_048_576,
            131_072,
            [3.0, 15.0, 0.3],
            kimi_levels(),
            kimi_compat(),
        ),
        kimi_model(
            "k3-256k",
            "Kimi K3-256K",
            262_144,
            131_072,
            [0.0, 0.0, 0.0],
            kimi_levels(),
            kimi_compat(),
        ),
        kimi_model(
            "kimi-for-coding-highspeed",
            "Kimi For Coding HighSpeed",
            262_144,
            32_768,
            [1.9, 8.0, 0.38],
            json!({"off": null}),
            kimi_highspeed_compat(),
        ),
    ];
    models.extend(kimi_models_not_in_pi());
    json!({
        "name": "Kimi For Coding (overseas)",
        "baseUrl": moonshot::CODING_BASE_URL,
        "api": "openai-completions",
        "apiKey": format!("${}", moonshot::KIMI_KEY_ENV),
        "models": models,
    })
}

/// Add the Kimi Code models that pi 1.4.0 lacks to pi's built-in
/// `kimi-coding` provider (the `coding-cn` plan).
///
/// A built-in provider id as a key of `models.json` changes that provider.
/// A `models` entry adds a model, and the model takes `api` and `baseUrl`
/// from the built-in models (pi docs `models-configuration.md`,
/// "Providers" and "Models"; `dist/core/provider-composer.js`,
/// `applyModelsJson`). The other fields of a shared entry stay. A shared
/// model with the same id wins.
fn add_kimi_coding_cn_models(providers: &mut Value) {
    let id = terrarium_core::moonshot::PI_CODING_CN_PROVIDER;
    if !providers.get(id).is_some_and(Value::is_object) {
        providers[id] = json!({});
    }
    let entry = &mut providers[id];
    if !entry.get("models").is_some_and(Value::is_array) {
        entry["models"] = json!([]);
    }
    let Some(list) = entry["models"].as_array_mut() else {
        return;
    };
    for m in kimi_models_not_in_pi() {
        if !list.iter().any(|e| e.get("id") == m.get("id")) {
            list.push(m);
        }
    }
}

/// The agent's `models.json`: the shared file (or none) with the
/// Terrarium providers set ([`TERRARIUM_PROVIDERS`]), and with the missing
/// Kimi Code models added to pi's built-in `kimi-coding`.
pub fn agent_models(shared: Option<Value>) -> Value {
    agent_models_for(shared, None, None)
}

/// [`agent_models`], and for an agent on a self-hosted server
/// (`terrarium_core::selfhosted`) the custom provider
/// `terrarium-selfhosted` ([`terrarium_core::selfhosted::SelfHosted::pi_provider`])
/// with the agent's `model` in its model list. A provider of that id in
/// the shared file is replaced; an agent on another subscription gets no
/// such provider.
pub fn agent_models_for(
    shared: Option<Value>,
    selfhosted: Option<&terrarium_core::selfhosted::SelfHosted>,
    model: Option<&str>,
) -> Value {
    let mut models = shared.unwrap_or_else(|| json!({}));
    if !models.get("providers").is_some_and(Value::is_object) {
        models["providers"] = json!({});
    }
    let id = terrarium_core::selfhosted::PI_PROVIDER;
    if let Some(map) = models["providers"].as_object_mut() {
        map.remove(id);
    }
    if let Some(sh) = selfhosted {
        let extra: Vec<String> = sh.model_for(model).into_iter().collect();
        let extra: Vec<&str> = extra.iter().map(String::as_str).collect();
        models["providers"][id] = sh.pi_provider(&extra);
    }
    models["providers"][terrarium_core::zai::PI_API_PROVIDER] = zai_api_provider();
    models["providers"][terrarium_core::moonshot::PI_CODING_PROVIDER] = kimi_coding_intl_provider();
    add_kimi_coding_cn_models(&mut models["providers"]);
    models
}

/// Write the per-agent files and return the overlays.
///
/// - `home` is the shared pi home on the host (the daemon's view).
/// - `prompt`: when `Some`, write it to `prompt.md`. When `None`, keep the
///   file that is there (the daemon renders it).
///
/// The extension is written on every start, so a new adapter version
/// ships a new extension. It must exist before pi starts: a missing `-e`
/// path stops pi.
pub fn materialize(
    opts: &PiOptions,
    agent: &Agent,
    home: &Path,
    prompt: Option<&str>,
) -> Result<Vec<FileOverlay>> {
    materialize_for(opts, agent, home, prompt, None)
}

/// [`materialize`] for an agent on `selfhosted` (its subscription's
/// server, `terrarium_core::selfhosted`): its `models.json` gets the
/// provider `terrarium-selfhosted` ([`agent_models_for`]).
pub fn materialize_for(
    opts: &PiOptions,
    agent: &Agent,
    home: &Path,
    prompt: Option<&str>,
    selfhosted: Option<&terrarium_core::selfhosted::SelfHosted>,
) -> Result<Vec<FileOverlay>> {
    let dir = pi_dir(agent);
    let sessions = sessions_dir(agent);
    for d in [&dir, &sessions] {
        fs::create_dir_all(d).map_err(|e| PiError::io(d, e))?;
    }
    if let Some(text) = prompt {
        write_atomic(&prompt_path(agent), text.as_bytes())?;
    }
    let ext = extension_path(agent);
    write_atomic(&ext, EXTENSION.as_bytes())?;

    // settings.json, in layers (PLAN §8.2): the agent's copy (a copy of
    // the shared file on the first start), then the user settings file
    // (`<config repo>/harness/pi/settings.json`) merged over it, then the
    // forced key: every start sets `lastChangelogVersion` to the pinned
    // version, so the "What's new" changelog never stops the start. The
    // per-agent options (provider, model, prompt) are flags.
    let own = dir.join(SETTINGS_FILE);
    let shared = home.join(SETTINGS_FILE);
    let current = read_json(&own)?;
    let mut settings = match &current {
        Some(v) => v.clone(),
        None => read_json(&shared)?.unwrap_or_else(|| Value::Object(Default::default())),
    };
    if let Some(user @ Value::Object(_)) = &agent.harness_settings {
        terrarium_core::harness_settings::deep_merge(&mut settings, user);
    }
    settings[LAST_CHANGELOG_KEY] = Value::String(opts.flags.verified.to_owned());
    terrarium_core::harness_settings::deep_merge(&mut settings, &forced_settings());
    if current.as_ref() != Some(&settings) {
        let bytes =
            serde_json::to_vec_pretty(&settings).map_err(|e| PiError::json("settings.json", e))?;
        write_atomic(&own, &bytes)?;
    }
    // bwrap needs a mount point. Otherwise it makes an empty file and
    // leaves it behind.
    if !shared.exists() {
        write_atomic(&shared, b"{}\n")?;
    }

    // models.json: the shared file and the Terrarium providers, on every
    // start. pi needs `providers` in the file, also in the mount point.
    let own_models = dir.join(MODELS_FILE);
    let shared_models = home.join(MODELS_FILE);
    let models = agent_models_for(
        read_json(&shared_models)?,
        selfhosted,
        agent.model.as_deref(),
    );
    if read_json(&own_models)?.as_ref() != Some(&models) {
        let bytes =
            serde_json::to_vec_pretty(&models).map_err(|e| PiError::json("models.json", e))?;
        write_atomic(&own_models, &bytes)?;
    }
    if !shared_models.exists() {
        write_atomic(&shared_models, b"{\"providers\": {}}\n")?;
    }

    // B1: empty prompt files over the operator's (and a mount point for
    // each in the shared home). Written again at each start.
    let mut prompt_overlays = Vec::new();
    for name in DISCOVERED_PROMPT_FILES {
        let own = dir.join(name);
        write_atomic(&own, b"")?;
        let shared = home.join(name);
        if !shared.exists() {
            write_atomic(&shared, b"")?;
        }
        prompt_overlays.push(FileOverlay {
            source: own,
            target: opts.agent_dir.join(name),
            writable: false,
        });
    }

    let mut overlays = vec![
        FileOverlay {
            source: own,
            target: opts.agent_dir.join(SETTINGS_FILE),
            // pi writes settings (for example after `/model`).
            writable: true,
        },
        FileOverlay {
            source: own_models,
            target: opts.agent_dir.join(MODELS_FILE),
            writable: false,
        },
        FileOverlay {
            source: ext.clone(),
            target: ext,
            writable: false,
        },
        // pi writes its session files here; the rest of the harness
        // directory is read-only.
        FileOverlay {
            source: sessions.clone(),
            target: sessions,
            writable: true,
        },
    ];
    overlays.extend(prompt_overlays);
    Ok(overlays)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_agent_in;

    /// The user file merges over the agent's copy at each start; the
    /// forced changelog key wins.
    #[test]
    fn user_settings_merge_under_the_forced_key() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-pi");
        fs::create_dir_all(&home).expect("home");
        fs::write(
            home.join(SETTINGS_FILE),
            r#"{"defaultThinkingLevel":"off","theme":"light"}"#,
        )
        .expect("shared");
        let mut agent = test_agent_in(tmp.path(), None);
        agent.harness_settings = Some(json!({"theme": "dark", LAST_CHANGELOG_KEY: "0.0.1"}));
        let opts = PiOptions::default();
        materialize(&opts, &agent, &home, None).expect("materialize");
        let v: Value =
            serde_json::from_slice(&fs::read(pi_dir(&agent).join(SETTINGS_FILE)).expect("read"))
                .expect("json");
        assert_eq!(v["defaultThinkingLevel"], "off");
        assert_eq!(v["theme"], "dark");
        assert_eq!(v[LAST_CHANGELOG_KEY], opts.flags.verified);
    }

    /// B1: the operator's `SYSTEM.md` and `APPEND_SYSTEM.md` stay in the
    /// shared home, and the agent sees empty files over them.
    #[test]
    fn the_shared_prompt_files_are_empty_for_the_agent() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-pi");
        fs::create_dir_all(&home).expect("home");
        fs::write(home.join("SYSTEM.md"), "operator system").expect("w");
        let agent = test_agent_in(tmp.path(), None);
        let opts = PiOptions::default();
        let overlays = materialize(&opts, &agent, &home, Some("p")).expect("m");
        for name in ["SYSTEM.md", "APPEND_SYSTEM.md"] {
            let o = overlays
                .iter()
                .find(|o| o.target == opts.agent_dir.join(name))
                .unwrap_or_else(|| panic!("{name}"));
            assert!(!o.writable, "{name}");
            assert!(o.source.starts_with(pi_dir(&agent)), "{name}");
            assert_eq!(fs::read(&o.source).expect("own"), b"", "{name}");
            // A mount point in the shared home, with the operator's text kept.
            assert!(home.join(name).is_file(), "{name}");
        }
        assert_eq!(
            fs::read_to_string(home.join("SYSTEM.md")).expect("shared"),
            "operator system"
        );
        // A stale text in the agent's file goes at the next start.
        fs::write(pi_dir(&agent).join("SYSTEM.md"), "x").expect("w");
        materialize(&opts, &agent, &home, None).expect("again");
        assert_eq!(
            fs::read(pi_dir(&agent).join("SYSTEM.md")).expect("own"),
            b""
        );
    }

    /// The forced keys win over the user file and the agent's copy.
    #[test]
    fn forced_settings_win() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-pi");
        fs::create_dir_all(&home).expect("home");
        let mut agent = test_agent_in(tmp.path(), None);
        agent.harness_settings = Some(json!({
            "doubleEscapeAction": "tree",
            "branchSummary": {"skipPrompt": false, "reserveTokens": 1},
            "tuiMode": "fullscreen",
            "cacheWarming": "idle",
        }));
        materialize(&PiOptions::default(), &agent, &home, None).expect("m");
        let v: Value =
            serde_json::from_slice(&fs::read(pi_dir(&agent).join(SETTINGS_FILE)).expect("r"))
                .expect("json");
        assert_eq!(v["doubleEscapeAction"], "none");
        assert_eq!(
            v["branchSummary"],
            json!({"skipPrompt": true, "reserveTokens": 1})
        );
        assert_eq!(v["tuiMode"], "regular");
        assert_eq!(v["defaultProjectTrust"], "never");
        assert_eq!(v["cacheWarming"], "idle");
    }

    /// The packaged default holds no forced key.
    #[test]
    fn the_packaged_default_has_no_forced_key() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../defaults/harness/pi/settings.json");
        let v = terrarium_core::harness_settings::load(&path, terrarium_core::HarnessId::Pi)
            .expect("parses")
            .expect("exists");
        let forced = forced_settings();
        for key in v.as_object().expect("object").keys() {
            assert!(forced.get(key).is_none(), "{key} is forced");
            assert_ne!(key, LAST_CHANGELOG_KEY);
        }
    }

    #[test]
    fn materialize_writes_extension_settings_and_overlays() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-pi");
        fs::create_dir_all(&home).expect("home");
        fs::write(
            home.join(SETTINGS_FILE),
            r#"{"defaultThinkingLevel":"off"}"#,
        )
        .expect("shared");
        fs::write(home.join(AUTH_FILE), "{}").expect("auth");
        let agent = test_agent_in(tmp.path(), None);
        let opts = PiOptions::default();

        let overlays = materialize(&opts, &agent, &home, Some("hello")).expect("materialize");
        let ext = extension_path(&agent);
        assert_eq!(fs::read_to_string(&ext).expect("ext"), EXTENSION);
        assert!(sessions_dir(&agent).is_dir());
        assert_eq!(prompt_file(&agent), Some(prompt_path(&agent)));
        assert_eq!(
            overlays,
            vec![
                FileOverlay {
                    source: pi_dir(&agent).join(SETTINGS_FILE),
                    target: PathBuf::from("/auth/pi/settings.json"),
                    writable: true,
                },
                FileOverlay {
                    source: pi_dir(&agent).join(MODELS_FILE),
                    target: PathBuf::from("/auth/pi/models.json"),
                    writable: false,
                },
                FileOverlay {
                    source: ext.clone(),
                    target: ext.clone(),
                    writable: false,
                },
                FileOverlay {
                    source: sessions_dir(&agent),
                    target: sessions_dir(&agent),
                    writable: true,
                },
                FileOverlay {
                    source: pi_dir(&agent).join("SYSTEM.md"),
                    target: PathBuf::from("/auth/pi/SYSTEM.md"),
                    writable: false,
                },
                FileOverlay {
                    source: pi_dir(&agent).join("APPEND_SYSTEM.md"),
                    target: PathBuf::from("/auth/pi/APPEND_SYSTEM.md"),
                    writable: false,
                },
            ]
        );
        let own: Value =
            serde_json::from_slice(&fs::read(&overlays[0].source).expect("own")).expect("json");
        assert_eq!(own["defaultThinkingLevel"], "off");
        assert_eq!(own[LAST_CHANGELOG_KEY], opts.flags.verified);

        // A restart keeps the agent's settings, writes the extension again,
        // and keeps the prompt.
        fs::write(
            &overlays[0].source,
            r#"{"defaultModel":"x","lastChangelogVersion":"0.85.1"}"#,
        )
        .expect("write");
        fs::write(&ext, "tampered").expect("tamper");
        materialize(&opts, &agent, &home, None).expect("again");
        let own: Value =
            serde_json::from_slice(&fs::read(&overlays[0].source).expect("own")).expect("json");
        assert_eq!(own["defaultModel"], "x");
        assert_eq!(own[LAST_CHANGELOG_KEY], opts.flags.verified);
        assert_eq!(fs::read_to_string(&ext).expect("ext"), EXTENSION);
        assert_eq!(
            fs::read_to_string(prompt_path(&agent)).expect("prompt"),
            "hello"
        );

        // Auth and the shared settings stay as they were. No overlay covers
        // auth.json. models.json is the agent's read-only copy.
        assert_eq!(
            fs::read_to_string(home.join(AUTH_FILE)).expect("auth"),
            "{}"
        );
        assert_eq!(
            fs::read_to_string(home.join(SETTINGS_FILE)).expect("shared"),
            r#"{"defaultThinkingLevel":"off"}"#
        );
        let auth = Path::new("/auth/pi").join(AUTH_FILE);
        assert!(!overlays.iter().any(|o| auth.starts_with(&o.target)));
    }

    #[test]
    fn models_overlay_keeps_shared_providers_and_adds_zai_api_and_kimi_intl() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-pi");
        fs::create_dir_all(&home).expect("home");
        let shared = r#"{"providers":{"local":{"baseUrl":"http://l/v1","api":"openai-completions","models":[{"id":"m"}]},"zai-api":{"baseUrl":"http://old"}}}"#;
        fs::write(home.join(MODELS_FILE), shared).expect("shared");
        let agent = test_agent_in(tmp.path(), None);
        let opts = PiOptions::default();
        let overlays = materialize(&opts, &agent, &home, None).expect("materialize");
        let o = overlays
            .iter()
            .find(|o| o.target == opts.agent_dir.join(MODELS_FILE))
            .expect("models overlay");
        assert!(!o.writable);
        let own: Value = serde_json::from_slice(&fs::read(&o.source).expect("own")).expect("json");
        assert_eq!(own["providers"]["local"]["baseUrl"], "http://l/v1");
        let z = &own["providers"]["zai-api"];
        assert_eq!(z["baseUrl"], "https://api.z.ai/api/paas/v4");
        assert_eq!(z["api"], "openai-completions");
        assert_eq!(z["apiKey"], "$ZAI_API_KEY");
        assert_eq!(z["models"][0]["id"], "glm-5.3-flash");
        assert_eq!(z["models"][1]["id"], "glm-5.3");
        let k = &own["providers"]["kimi-coding-intl"];
        assert_eq!(k["baseUrl"], "https://api.kimi.ai/coding/v1");
        assert_eq!(k["api"], "openai-completions");
        assert_eq!(k["apiKey"], "$KIMI_API_KEY");
        assert_eq!(k["models"][0]["id"], "kimi-for-coding");
        assert_eq!(k["models"][0]["compat"]["thinkingFormat"], "openai");
        // The overseas provider has every documented Kimi Code id, and
        // the highspeed model has the DeepSeek format of pi's catalog.
        let intl_ids: Vec<&str> = k["models"]
            .as_array()
            .expect("models")
            .iter()
            .map(|m| m["id"].as_str().expect("id"))
            .collect();
        for id in terrarium_core::moonshot::CODING_MODELS {
            assert!(intl_ids.contains(&id), "{id} in {intl_ids:?}");
        }
        let hs = k["models"]
            .as_array()
            .expect("models")
            .iter()
            .find(|m| m["id"] == "kimi-for-coding-highspeed")
            .expect("highspeed");
        assert_eq!(hs["compat"]["thinkingFormat"], "deepseek");
        // pi's built-in `kimi-coding` (plan `coding-cn`) gets the ids that
        // its catalog lacks, with no base URL of its own.
        let cn = &own["providers"]["kimi-coding"];
        assert!(cn.get("baseUrl").is_none(), "{cn}");
        assert_eq!(cn["models"][0]["id"], "k3.1-pro");
        assert_eq!(cn["models"][1]["id"], "k3.1-pro-highspeed");
        assert_eq!(cn["models"][0]["thinkingLevelMap"]["off"], Value::Null);
        assert_eq!(cn["models"][0]["contextWindow"], 1_048_576);
        // The shared file stays as it was.
        assert_eq!(
            fs::read_to_string(home.join(MODELS_FILE)).expect("shared"),
            shared
        );
        // No shared file: the copy has only the Terrarium providers, and
        // the mount point is a valid empty file for pi.
        fs::remove_file(home.join(MODELS_FILE)).expect("rm");
        materialize(&opts, &agent, &home, None).expect("again");
        let own: Value = serde_json::from_slice(&fs::read(&o.source).expect("own")).expect("json");
        let mut ids: Vec<&String> = own["providers"].as_object().expect("obj").keys().collect();
        ids.sort();
        assert_eq!(ids, ["kimi-coding", "kimi-coding-intl", "zai-api"]);
        let cn = own["providers"]["kimi-coding"].as_object().expect("obj");
        assert_eq!(cn.keys().collect::<Vec<_>>(), ["models"]);
        let mount: Value =
            serde_json::from_slice(&fs::read(home.join(MODELS_FILE)).expect("mount"))
                .expect("json");
        assert_eq!(mount, json!({"providers": {}}));
        for p in TERRARIUM_PROVIDERS {
            assert!(own["providers"].get(*p).is_some(), "{p}");
        }
    }

    #[test]
    fn shared_kimi_coding_entry_keeps_its_fields_and_models() {
        let shared = json!({"providers": {"kimi-coding": {
            "headers": {"X-A": "1"},
            "models": [{"id": "k3.1-pro", "name": "mine"}, {"id": "other"}],
        }}});
        let own = agent_models(Some(shared));
        let cn = &own["providers"]["kimi-coding"];
        assert_eq!(cn["headers"]["X-A"], "1");
        let ids: Vec<&str> = cn["models"]
            .as_array()
            .expect("models")
            .iter()
            .map(|m| m["id"].as_str().expect("id"))
            .collect();
        assert_eq!(ids, ["k3.1-pro", "other", "k3.1-pro-highspeed"]);
        assert_eq!(cn["models"][0]["name"], "mine");
        // A second pass adds nothing.
        let again = agent_models(Some(own.clone()));
        assert_eq!(again, own);
    }

    #[test]
    fn materialize_makes_missing_mount_point_and_no_prompt() {
        let tmp = tempfile::tempdir().expect("tmp");
        let home = tmp.path().join("auth-pi");
        fs::create_dir_all(&home).expect("home");
        let agent = test_agent_in(tmp.path(), None);
        materialize(&PiOptions::default(), &agent, &home, None).expect("materialize");
        assert_eq!(
            fs::read_to_string(home.join(SETTINGS_FILE)).expect("shared"),
            "{}\n"
        );
        assert_eq!(prompt_file(&agent), None);
        fs::write(prompt_path(&agent), " \n").expect("empty prompt");
        assert_eq!(prompt_file(&agent), None);
    }
}
