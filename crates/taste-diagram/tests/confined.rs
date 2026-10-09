//! The helper as the IDE runs it: confined, over its pipes.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

fn helper() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_taste-diagram"))
}

fn confinable() -> bool {
    taste_confine::landlock_abi().is_some_and(|abi| abi >= 3)
}

/// Started without confinement, the helper refuses and draws nothing.
#[test]
fn unconfined_it_refuses() {
    let output = Command::new(helper())
        .stdin(Stdio::null())
        .output()
        .expect("the helper starts");
    assert_eq!(output.status.code(), Some(2));
    let said = String::from_utf8_lossy(&output.stdout);
    assert!(said.contains("runs only confined"), "{said}");
}

/// Confined as the IDE confines it, it reads its fonts, says it is ready,
/// and answers a diagram with exactly the pixels its header promises — and
/// a broken diagram with the parser's reason, staying up for the next.
#[test]
fn confined_it_draws() {
    if !confinable() {
        eprintln!("no Landlock ABI 3 here; the IDE would show the code instead");
        return;
    }
    let policy = taste_confine::Policy::for_program(helper())
        .unwrap()
        .with_fonts();
    let mut command = Command::new(helper());
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    taste_confine::confine(&mut command, &policy).unwrap();
    let mut child = command.spawn().expect("the helper starts");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert!(line.contains("\"ready\":true"), "{line}");

    let ask = |stdin: &mut std::process::ChildStdin, source: &str| {
        let request = serde_json::json!({
            "source": source, "dark": true, "family": "Adwaita Sans", "scale": 2.0
        });
        writeln!(stdin, "{request}").unwrap();
        stdin.flush().unwrap();
    };
    ask(&mut stdin, "flowchart LR\n  A[Start] -->|go| B[Done]");
    line.clear();
    stdout.read_line(&mut line).unwrap();
    let header: serde_json::Value = serde_json::from_str(&line).expect(&line);
    let (width, height) = (
        header["width"].as_u64().expect(&line),
        header["height"].as_u64().unwrap(),
    );
    assert!(width > 100 && height > 20, "{header}");
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    stdout.read_exact(&mut pixels).unwrap();
    assert!(
        pixels.chunks(4).any(|px| px[3] == 255),
        "something was drawn"
    );

    ask(&mut stdin, "flowchart LR\n  A -->");
    line.clear();
    stdout.read_line(&mut line).unwrap();
    assert!(line.contains("error") && line.contains("parse"), "{line}");

    drop(stdin);
    assert!(child.wait().unwrap().success());
}
