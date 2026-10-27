//! The helper as the IDE runs it: confined, over its pipes.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn helper() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_taste-archive"))
}

fn confinable() -> bool {
    taste_confine::landlock_abi().is_some_and(|abi| abi >= 3)
}

/// Started without confinement, the helper refuses and reads nothing.
#[test]
fn unconfined_it_refuses() {
    let output = Command::new(helper())
        .arg("wacz")
        .stdin(Stdio::null())
        .output()
        .expect("the helper starts");
    assert_eq!(output.status.code(), Some(2));
    let said = String::from_utf8_lossy(&output.stdout);
    assert!(said.contains("runs only confined"), "{said}");
}

/// Confined as the IDE confines it, it unpacks a WACZ into an index line
/// and bodies of exactly the lengths the index gives, and says why for a
/// file that is not one.
#[test]
fn confined_it_unpacks_a_wacz() {
    if !confinable() {
        eprintln!("no Landlock ABI 3 here; the IDE would not open archives");
        return;
    }
    let run = |format: &str, input: Vec<u8>| -> (serde_json::Value, Vec<u8>) {
        let policy = taste_confine::Policy::for_program(helper()).unwrap();
        let mut command = Command::new(helper());
        command
            .arg(format)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        taste_confine::confine(&mut command, &policy).unwrap();
        let mut child = command.spawn().expect("the helper starts");
        let mut stdin = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let mut rest = Vec::new();
        stdout.read_to_end(&mut rest).unwrap();
        writer.join().unwrap();
        child.wait().unwrap();
        (serde_json::from_str(&line).unwrap(), rest)
    };
    let wacz = std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../build-aux/headless/fixtures/archives/page.wacz"),
    )
    .unwrap();
    let (index, bodies) = run("wacz", wacz);
    assert_eq!(index["pages"].as_array().map(Vec::len), Some(2), "{index}");
    let lengths: usize = index["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["len"].as_u64().unwrap() as usize)
        .sum();
    assert_eq!(lengths, bodies.len());
    let (refused, nothing) = run("wacz", b"not a zip".to_vec());
    assert!(
        refused["error"].as_str().is_some_and(|e| e.contains("zip")),
        "{refused}"
    );
    assert!(nothing.is_empty());
}
