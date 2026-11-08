//! Shared integration binary. Keep module sources beside the standalone tests.
//! Run a module with: cargo test -p terrariumd --test it board::

#[path = "../common/mod.rs"]
mod common;
mod ter221_latency;
mod workspace_sync;

mod reservations;
mod reservations_api;

#[path = "../agent_interrupt.rs"]
mod agent_interrupt;
#[path = "../agent_keys.rs"]
mod agent_keys;
#[path = "../architect.rs"]
mod architect;
#[path = "../artifacts.rs"]
mod artifacts;
#[path = "../attachments.rs"]
mod attachments;
#[path = "../audit_stream.rs"]
mod audit_stream;
#[path = "../backups.rs"]
mod backups;
#[path = "../board.rs"]
mod board;
#[path = "../bookmarks.rs"]
mod bookmarks;
#[path = "../bound_agents.rs"]
mod bound_agents;
#[path = "build_inputs.rs"]
mod build_inputs;
#[path = "../calendar.rs"]
mod calendar;
#[path = "../card_dispatcher.rs"]
mod card_dispatcher;
#[path = "../card_files.rs"]
mod card_files;
#[path = "../card_reminder.rs"]
mod card_reminder;
#[path = "../card_sessions.rs"]
mod card_sessions;
#[path = "../config_repo.rs"]
mod config_repo;
#[path = "../db.rs"]
mod db;
#[path = "../displays.rs"]
mod displays;
#[path = "../event_pages.rs"]
mod event_pages;
#[path = "../event_sources.rs"]
mod event_sources;
#[path = "../exec_redaction.rs"]
mod exec_redaction;
#[path = "../fleet.rs"]
mod fleet;
#[path = "../git_proxy.rs"]
mod git_proxy;
#[path = "../github_gate.rs"]
mod github_gate;
#[path = "../global_secret_rule.rs"]
mod global_secret_rule;
#[path = "../harness_auto_update.rs"]
mod harness_auto_update;
#[path = "../harness_tools.rs"]
mod harness_tools;
#[path = "../hosts.rs"]
mod hosts;
#[path = "../idle_alert.rs"]
mod idle_alert;
#[path = "../inbox.rs"]
mod inbox;
mod inbox_perf;
#[path = "../input_watchdog.rs"]
mod input_watchdog;
#[path = "../jobs.rs"]
mod jobs;
#[path = "../lifecycle.rs"]
mod lifecycle;
#[path = "../log_sessions.rs"]
mod log_sessions;
#[path = "../mail.rs"]
mod mail;
#[path = "../memory.rs"]
mod memory;
#[path = "../monitors.rs"]
mod monitors;
#[path = "../password_cli.rs"]
mod password_cli;
#[path = "../pins.rs"]
mod pins;
#[path = "../pipelines.rs"]
mod pipelines;
#[path = "../policy_writes.rs"]
mod policy_writes;
#[path = "../preferences.rs"]
mod preferences;
#[path = "../project_actions.rs"]
mod project_actions;
#[path = "../project_folders.rs"]
mod project_folders;
#[path = "../projects_agents.rs"]
mod projects_agents;
#[path = "../question_guard.rs"]
mod question_guard;
#[path = "../questions.rs"]
mod questions;
#[path = "../queued_turns.rs"]
mod queued_turns;
#[path = "../redaction.rs"]
mod redaction;
#[path = "../repos.rs"]
mod repos;
#[path = "../resets.rs"]
mod resets;
#[path = "../rules.rs"]
mod rules;
#[path = "../rules_engine.rs"]
mod rules_engine;
#[path = "../rules_tools.rs"]
mod rules_tools;
#[path = "../rules_verbs.rs"]
mod rules_verbs;
#[path = "../run_secrets.rs"]
mod run_secrets;
#[path = "../runner_process.rs"]
mod runner_process;
#[path = "../runtime.rs"]
mod runtime;
#[path = "../schedules.rs"]
mod schedules;
#[path = "../screen.rs"]
mod screen;
#[path = "../secret_grants.rs"]
mod secret_grants;
#[path = "../secrets.rs"]
mod secrets;
#[path = "../self_update.rs"]
mod self_update;
#[path = "../skills.rs"]
mod skills;
#[path = "../spawn_rights.rs"]
mod spawn_rights;
#[path = "../spawns.rs"]
mod spawns;
#[path = "../subscriptions.rs"]
mod subscriptions;
#[path = "../system_prompts.rs"]
mod system_prompts;
#[path = "../target_keys.rs"]
mod target_keys;
#[path = "../template_files.rs"]
mod template_files;
#[path = "../templates.rs"]
mod templates;
#[path = "../test_runs.rs"]
mod test_runs;
#[path = "../thread_agents.rs"]
mod thread_agents;
#[path = "../thread_cards.rs"]
mod thread_cards;
#[path = "../thread_close.rs"]
mod thread_close;
#[path = "../thread_tracking.rs"]
mod thread_tracking;
#[path = "../thread_wake.rs"]
mod thread_wake;
#[path = "../threads.rs"]
mod threads;
#[path = "../time_params.rs"]
mod time_params;
#[path = "../tool_runs.rs"]
mod tool_runs;
#[path = "../tools.rs"]
mod tools;
#[path = "../transcripts.rs"]
mod transcripts;
#[path = "../transcripts_live.rs"]
mod transcripts_live;
#[path = "../triggers.rs"]
mod triggers;
#[path = "../turn_tags.rs"]
mod turn_tags;
#[path = "../usage_cards.rs"]
mod usage_cards;
#[path = "../waits.rs"]
mod waits;
#[path = "../webhooks.rs"]
mod webhooks;
#[path = "../wiki.rs"]
mod wiki;
