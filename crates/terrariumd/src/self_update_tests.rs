#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn the_policy_and_the_channel_parse() {
    assert_eq!("".parse::<Policy>().unwrap(), Policy::Notify);
    assert_eq!("AUTO".parse::<Policy>().unwrap(), Policy::Auto);
    assert_eq!("off".parse::<Policy>().unwrap(), Policy::Off);
    assert!("sometimes".parse::<Policy>().is_err());
    assert_eq!("".parse::<Channel>().unwrap(), Channel::Stable);
    assert_eq!("pre".parse::<Channel>().unwrap(), Channel::Pre);
    assert!("nightly".parse::<Channel>().is_err());
    assert_eq!(Channel::Stable.tag(), "latest");
    assert_eq!(Channel::Pre.tag(), "pre");
    assert_eq!(
        releases_url("o/r"),
        "https://api.github.com/repos/o/r/releases?per_page=30"
    );
}

fn gh(tag: &str, pre: bool, draft: bool) -> serde_json::Value {
    json!({"tag_name": tag, "name": tag, "prerelease": pre, "draft": draft,
           "body": "## Changes", "html_url": format!("https://x/{tag}"),
           "published_at": "2026-11-01T00:00:00Z"})
}

#[test]
fn the_newest_release_of_the_channel_is_the_highest_semver() {
    let v = json!([
        gh("v0.2.0", false, false),
        gh("v0.10.0", false, false),
        gh("v0.11.0-alpha", true, false),
        gh("v1.0.0", false, true),
        gh("not-a-version", false, false),
    ]);
    let r = parse_releases(&v);
    assert_eq!(r.len(), 3, "no draft, no bad tag");
    assert_eq!(r[0].version, "0.2.0");
    assert_eq!(r[0].notes, "## Changes");
    assert_eq!(newest(&r, Channel::Stable).unwrap().version, "0.10.0");
    assert_eq!(newest(&r, Channel::Pre).unwrap().version, "0.11.0-alpha");
    assert!(newest(&[], Channel::Stable).is_none());
    assert!(parse_releases(&json!({"message": "rate limited"})).is_empty());
}

#[test]
fn newer_compares_semver_and_never_a_downgrade() {
    assert!(is_newer("0.2.0", "0.1.0"));
    assert!(is_newer("0.10.0", "0.9.0"));
    assert!(is_newer("0.2.0", "0.2.0-alpha"));
    assert!(!is_newer("0.1.0", "0.1.0"));
    assert!(!is_newer("0.1.0", "0.2.0"));
    assert!(!is_newer("x", "0.1.0"));
}

fn sample_run(outcome: Outcome) -> Run {
    Run {
        id: "01R".into(),
        kind: RunKind::Update,
        actor: "agent:01SYS".into(),
        target: "0.2.0".into(),
        from: Some(terrarium_updater::protocol::Image {
            reference: "terrarium:dev".into(),
            version: Some("0.1.0".into()),
            ..Default::default()
        }),
        to: None,
        backup: Some("/state/backups/b.zip".into()),
        outcome,
        error: Some("health: unhealthy".into()),
        steps: Vec::new(),
        started_at: String::new(),
        ended_at: Some(String::new()),
    }
}

#[test]
fn the_mails_name_the_versions_and_the_way_back() {
    let (s, b) = release_mail(
        &Release {
            version: "0.2.0".into(),
            tag: "v0.2.0".into(),
            name: "v0.2.0".into(),
            prerelease: false,
            published_at: None,
            notes: String::new(),
            url: "https://x/v0.2.0".into(),
        },
        "0.1.0+abc",
        Policy::Notify,
        "http://d/settings/software",
    );
    assert_eq!(s, "Terrarium 0.2.0 is available");
    assert!(b.contains("This daemon runs 0.1.0+abc"), "{b}");
    assert!(b.contains("terrarium admin update apply --version 0.2.0"));
    assert!(b.ends_with("Open: http://d/settings/software\n"));

    let (s, b) = result_mail(&sample_run(Outcome::RollbackFailed), "l");
    assert_eq!(s, "ACTION NEEDED: Terrarium update to 0.2.0 failed");
    assert!(
        b.contains("from 0.1.0 to 0.2.0, asked by agent:01SYS"),
        "{b}"
    );
    assert!(b.contains("restore the backup by hand"), "{b}");
    let (s, b) = result_mail(&sample_run(Outcome::Succeeded), "l");
    assert_eq!(s, "Terrarium update to 0.2.0 succeeded");
    assert!(b.contains("terrarium admin update rollback"), "{b}");
}
