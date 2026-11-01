//! One Kimi turn through pi on Moonshot (docs/compat.md, "Moonshot").
//!
//! Ignored by default: it needs a Moonshot or Kimi Code key and spends a
//! little of it. Run:
//!
//! ```sh
//! MOONSHOT_API_KEY=<key> MOONSHOT_PLAN=coding \
//!   cargo test -p terrarium-adapter-pi --test moonshot_live -- --ignored
//! ```
//!
//! - `MOONSHOT_PLAN`: `coding` (default, the custom `kimi-coding-intl`
//!   provider from the agent `models.json`), `coding-cn` (pi's built-in
//!   `kimi-coding`), `api` (built-in `moonshotai`) or `api-cn` (built-in
//!   `moonshotai-cn`). A Kimi Code key works on the `coding` plans only;
//!   an Open Platform key works on the `api` plans only.
//! - `PI_BIN`: the pi binary. Default: `pi` on `PATH` (`./tools/bin/pi` in
//!   the repo after `terrariumd harness install`).
//!
//! The test builds the provider and model flags with the adapter's own
//! functions, writes the `models.json` that an agent gets, sets the env
//! that the daemon gives an agent on the subscription, and runs
//! `pi --print` in an empty `PI_CODING_AGENT_DIR` with the quiet flags.

use std::process::Command;

use terrarium_adapter_pi::{PI_PINNED, config, flags, provider_for};
use terrarium_core::HarnessId;
use terrarium_core::config::SubscriptionDef;
use terrarium_core::moonshot;

#[test]
#[ignore = "needs MOONSHOT_API_KEY and spends a Moonshot turn"]
fn one_kimi_turn_through_pi() {
    let Some(key) = std::env::var(moonshot::KEY_ENV)
        .ok()
        .filter(|k| !k.trim().is_empty())
    else {
        eprintln!("MOONSHOT_API_KEY is not set: nothing to run");
        return;
    };
    let plan = std::env::var("MOONSHOT_PLAN").unwrap_or_else(|_| "coding".into());
    let sub = SubscriptionDef {
        name: "kimi".into(),
        provider: moonshot::PROVIDER.into(),
        plan: Some(plan.clone()),
        harnesses: vec![HarnessId::Pi],
        probe: None,
        alert_at: 80,
        selfhosted: None,
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
        .env(moonshot::KEY_ENV, &key)
        .envs(moonshot::extra_env(
            moonshot::plan_or_default(Some(&plan)),
            &key,
        ))
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
