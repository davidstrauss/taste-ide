//! The helper as the IDE runs it: confined, over its pipes.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn helper() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_taste-taskfile"))
}

/// Started without confinement, the helper refuses and reads nothing.
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

/// Confined as the IDE confines it, it answers a request with each task's
/// variables, and a request it cannot read with a reason.
#[test]
fn confined_it_answers() {
    if !taste_confine::landlock_abi().is_some_and(|abi| abi >= 3) {
        eprintln!("no Landlock ABI 3 here; the IDE would ask for no variables");
        return;
    }
    let ask = |request: &[u8]| {
        let policy = taste_confine::Policy::for_program(helper()).unwrap();
        let mut command = Command::new(helper());
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        taste_confine::confine(&mut command, &policy).unwrap();
        let mut child = command.spawn().expect("the helper starts");
        child.stdin.take().unwrap().write_all(request).unwrap();
        let output = child.wait_with_output().unwrap();
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let request = serde_json::json!({
        "root": "/w/Taskfile.yml",
        "files": {
            "/w/Taskfile.yml":
                "version: '3'\ntasks:\n  fmt:\n    requires:\n      vars:\n        - FILE\n",
        },
        "tasks": [{"name": "fmt", "taskfile": "/w/Taskfile.yml", "line": 3, "column": 3}],
    });
    let reply = ask(request.to_string().as_bytes());
    assert_eq!(
        reply["tasks"]["fmt"]["vars"][0],
        serde_json::json!({"name": "FILE", "allowed": "any"}),
        "{reply}"
    );
    let reply = ask(b"not json");
    assert!(
        reply["error"].as_str().unwrap().contains("bad request"),
        "{reply}"
    );
}
