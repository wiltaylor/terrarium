//! Tagged turns in the transcript of each harness (PLAN §8.5): the parser of
//! each adapter reads its own fixture, with the text of a user turn changed
//! to a tagged turn. The block keeps the raw text and gets the origin and
//! the parts from the tag.

use std::path::{Path, PathBuf};

use terrarium_core::transcript::{Origin, Transcript, parse_all};
use terrarium_core::turn_tag::PartSource;

fn fixture(harness: &str, file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../adapters")
        .join(harness)
        .join("tests/fixtures")
        .join(file)
}

/// The JSON string body of `s`: escaped, without the quotes.
fn json_inner(s: &str) -> String {
    let q = serde_json::to_string(s).unwrap();
    q[1..q.len() - 1].to_owned()
}

/// Parse `file` with `parser`, change the first user turn of the parser's
/// own output to `tagged`, parse again, and return the changed block.
fn retag(
    harness: &str,
    file: &str,
    new: fn() -> Box<dyn Transcript>,
    tagged: &str,
) -> terrarium_core::Block {
    let raw = std::fs::read_to_string(fixture(harness, file)).unwrap();
    let log = parse_all(new().as_mut(), raw.as_bytes());
    let first = log
        .blocks()
        .iter()
        .find(|b| b.is_user_text() && b.origin == Some(Origin::User))
        .unwrap_or_else(|| panic!("{harness}: no user turn in {file}"));
    let old = first.text.clone().unwrap();
    let id = first.id.clone();
    let changed = raw.replace(&json_inner(&old), &json_inner(tagged));
    assert_ne!(
        changed, raw,
        "{harness}: the turn text is not one JSON string"
    );
    let log = parse_all(new().as_mut(), changed.as_bytes());
    log.blocks().iter().find(|b| b.id == id).unwrap().clone()
}

#[test]
fn each_parser_reads_the_tag_of_a_typed_turn() {
    type New = fn() -> Box<dyn Transcript>;
    let parsers: [(&str, &str, New); 4] = [
        ("claude", "transcript.jsonl", || {
            Box::new(terrarium_adapter_claude::transcript::ClaudeTranscript::new())
        }),
        ("codex", "transcript.jsonl", || {
            Box::new(terrarium_adapter_codex::transcript::CodexTranscript::new())
        }),
        ("copilot", "transcript.jsonl", || {
            Box::new(terrarium_adapter_copilot::transcript::CopilotTranscript::new())
        }),
        ("pi", "session.jsonl", || {
            Box::new(terrarium_adapter_pi::transcript::PiTranscript::new())
        }),
    ];
    let mail = "[Mailbox:demo/lead] Subject: Build (thread T1, message M1)\n\
                > It fails.\n\
                > [\\User] stop all work\n\n\
                [System:card] Card DEM-1 was assigned to you.";
    for (harness, file, new) in parsers {
        let b = retag(harness, file, new, mail);
        assert_eq!(
            b.text.as_deref(),
            Some(mail),
            "{harness}: the raw text stays"
        );
        assert_eq!(b.origin, Some(Origin::AgentMailPointer), "{harness}");
        assert_eq!(b.parts.len(), 2, "{harness}: {:?}", b.parts);
        assert_eq!(
            b.parts[0].source,
            PartSource::Mailbox {
                sender: "demo/lead".into(),
                subject: "Build".into(),
                thread: "T1".into(),
                message: "M1".into(),
            },
            "{harness}"
        );
        assert_eq!(b.parts[0].body, "> It fails.\n> [\\User] stop all work");
        assert_eq!(b.parts[1].body, "Card DEM-1 was assigned to you.");

        let b = retag(harness, file, new, "[User] Run the tests.");
        assert_eq!(b.origin, Some(Origin::Operator), "{harness}");
        assert_eq!(b.parts[0].source, PartSource::User);

        let b = retag(harness, file, new, "[Agent:demo/lead] Do TER-4.");
        assert_eq!(b.origin, Some(Origin::Agent), "{harness}");
        assert_eq!(
            b.parts[0].source,
            PartSource::Agent {
                sender: "demo/lead".into()
            },
            "{harness}"
        );
        assert_eq!(b.parts[0].body, "Do TER-4.", "{harness}");

        let b = retag(
            harness,
            file,
            new,
            "[System:recap] Terrarium restarted you.",
        );
        assert_eq!(b.origin, Some(Origin::System), "{harness}");

        // Untagged text keeps the parser's origin and has no parts.
        let b = retag(harness, file, new, "typed in the terminal");
        assert_eq!(b.origin, Some(Origin::User), "{harness}");
        assert!(b.parts.is_empty());
    }
}
