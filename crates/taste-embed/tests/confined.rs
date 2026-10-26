//! The helper refuses to run outside the confinement the IDE starts it in.
//! The confined path itself is `taste-semantic`'s
//! `embeds_confined_as_the_ide_starts_it`, which needs the model.

use std::path::Path;
use std::process::{Command, Stdio};

/// Started by hand, the helper refuses before it reads its model.
#[test]
fn unconfined_it_refuses() {
    let output = Command::new(Path::new(env!("CARGO_BIN_EXE_taste-embed")))
        .arg("/nonexistent.gguf")
        .stdin(Stdio::null())
        .output()
        .expect("the helper starts");
    assert_eq!(output.status.code(), Some(2));
    let said = String::from_utf8_lossy(&output.stdout);
    assert!(said.contains("runs only confined"), "{said}");
}
