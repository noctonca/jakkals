//! The transcript's line shape, its reasoning toggle and its file.

use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::scripted::TempDir;

fn reply_line(reasoning: Option<&str>) -> Line<'_> {
    Line {
        entry: Entry::Assistant {
            text: Some("Looking."),
            tool_calls: &[],
            reasoning: Some(reasoning),
        },
        step: 1,
        t_ms: 5,
    }
}

fn written(reasoning: bool, lines: Vec<Line<'_>>) -> Vec<String> {
    let mut out = Vec::new();
    let mut transcript = JsonLinesTranscript::new(&mut out, reasoning);
    for line in lines {
        transcript.record(line);
    }
    let text = String::from_utf8(out).expect("the transcript is UTF-8");
    assert!(text.ends_with('\n'), "every line ends");
    text.lines().map(str::to_owned).collect()
}

#[test]
fn reasoning_is_left_out_unless_asked_for() {
    assert_eq!(
        written(false, vec![reply_line(Some("Hmm."))]),
        [r#"{"role":"assistant","text":"Looking.","tool_calls":[],"step":1,"t_ms":5}"#]
    );
}

#[test]
fn asked_for_reasoning_is_kept_and_its_absence_is_null() {
    assert_eq!(
        written(true, vec![reply_line(Some("Hmm.")), reply_line(None)]),
        [
            r#"{"role":"assistant","text":"Looking.","tool_calls":[],"reasoning":"Hmm.","step":1,"t_ms":5}"#,
            r#"{"role":"assistant","text":"Looking.","tool_calls":[],"reasoning":null,"step":1,"t_ms":5}"#,
        ]
    );
}

#[test]
fn the_file_is_new_and_its_owners_only() {
    let dir = TempDir::new("transcript-file");
    let path = dir.base.join("run.jsonl");
    let mut transcript = JsonLinesTranscript::new(create(&path).expect("created"), false);
    transcript.record(Line {
        entry: Entry::User { text: "Go." },
        step: 0,
        t_ms: 0,
    });

    let mode = std::fs::metadata(&path)
        .expect("exists")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(
        std::fs::read_to_string(&path).expect("readable"),
        "{\"role\":\"user\",\"text\":\"Go.\",\"step\":0,\"t_ms\":0}\n",
        "each line is flushed as it is recorded"
    );

    let refused = create(&path).expect_err("an existing path is refused");
    assert_eq!(refused.kind(), std::io::ErrorKind::AlreadyExists);
}
