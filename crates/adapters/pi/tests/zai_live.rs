//! One GLM turn through pi on Z.ai (docs/compat.md, "Z.ai").
//!
//! Ignored by default: it needs a Z.ai key and spends a little of it. Run:
//!
//! ```sh
//! ZAI_API_KEY=<key> cargo test -p terrarium-adapter-pi --test zai_live -- --ignored
//! ```
//!
//! - `ZAI_PLAN`: `coding` (default, pi's built-in `zai`) or `api` (the
//!   custom `zai-api` provider from the agent `models.json`).
//! - `PI_BIN`: the pi binary. Default: `pi` on `PATH` (`./tools/bin/pi` in
//!   the repo after `terrariumd harness install`).
//!
//! The test builds the provider and model flags with the adapter's own
//! functions, writes the `models.json` that an agent gets, and runs
//! `pi --print` in an empty `PI_CODING_AGENT_DIR` with the quiet flags.

use std::process::Command;

use terrarium_adapter_pi::{PI_PINNED, config, flags, provider_for};
use terrarium_core::HarnessId;
use terrarium_core::config::SubscriptionDef;

#[test]
#[ignore = "needs ZAI_API_KEY and spends a Z.ai turn"]
fn one_glm_turn_through_pi() {
    let Some(key) = std::env::var(terrarium_core::zai::KEY_ENV)
        .ok()
        .filter(|k| !k.trim().is_empty())
    else {
        eprintln!("ZAI_API_KEY is not set: nothing to run");
        return;
    };
    let plan = std::env::var("ZAI_PLAN").unwrap_or_else(|_| "coding".into());
    let sub = SubscriptionDef {
        name: "glm".into(),
        provider: "zai".into(),
        plan: Some(plan),
        harnesses: vec![HarnessId::Claude, HarnessId::Pi],
        probe: None,
        alert_at: 80,
        selfhosted: None,
        cloud: None,
    };
    let provider = provider_for(&sub).expect("a pi provider");
    let model = flags::default_model(&provider).expect("a default model");

    let home = tempfile::tempdir().expect("tmp");
    let models = config::agent_models(None);
    std::fs::write(
        home.path().join(config::MODELS_FILE),
        serde_json::to_vec_pretty(&models).expect("json"),
    )
    .expect("models.json");
    std::fs::write(home.path().join(config::AUTH_FILE), "{}").expect("auth.json");
    std::fs::write(home.path().join(config::SETTINGS_FILE), "{}").expect("settings.json");

    let f = &PI_PINNED;
    let bin = std::env::var("PI_BIN").unwrap_or_else(|_| "pi".into());
    let out = Command::new(bin)
        .current_dir(home.path())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home.path())
        .env(f.config_env, home.path())
        .env(terrarium_core::zai::KEY_ENV, key)
        .envs(f.quiet_env.iter().copied())
        .args([f.provider, &provider, f.model, model])
        .args([
            f.no_extensions,
            f.no_mcp,
            f.no_skills,
            f.no_prompt_templates,
            f.no_context_files,
            f.no_approve,
            f.offline,
            "--no-session",
            "--print",
            "Reply with the single word: pong",
        ])
        .output()
        .expect("run pi");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "pi exited {:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status
    );
    assert!(
        stdout.to_ascii_lowercase().contains("pong"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
}
