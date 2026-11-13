//! The reader held to Task: every fixture listed by the pinned Task, read
//! by `taste_taskfile::answer`, and each answer checked against what
//! `task --dry` does with it — the variables said to be required all
//! given, then each left out, then each given a value its `enum` refuses.
//! `--dry` checks `requires` and runs nothing, and its exit status says
//! which way it went: 0 to run, 206 for a variable missing, 207 for a
//! value not allowed. Only the status is read, never Task's wording.
//!
//! The ports are held to Task by running the tasks of `fixtures/ports`,
//! each of which prints the `PORT` its commands are given: a task said to
//! serve a port is given that port.
//!
//! Task is in the devcontainer image at `IDE_TASK_VERSION`
//! (`.devcontainer/Containerfile`), so this runs wherever the gate does;
//! `TASTE_TASK_BIN` names another.

use std::path::{Path, PathBuf};
use std::process::Command;

use std::collections::BTreeMap;

use taste_taskfile::{Allowed, Found, Request, Site};

/// A value no fixture's `enum` allows.
const REFUSED: &str = "not-a-value";

fn task_bin() -> PathBuf {
    if let Some(named) = std::env::var_os("TASTE_TASK_BIN") {
        return PathBuf::from(named);
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|dir| dir.join("task"))
        .find(|task| task.is_file())
        .expect(
            "no `task` on PATH: the devcontainer image carries the pinned one — rebuild it \
             (podman build -t taste-ide-devcontainer .devcontainer), or set TASTE_TASK_BIN",
        )
}

/// `task` in `dir` with `args`, with nothing from this process's
/// environment, which Task would read variables from.
fn task(dir: &Path, args: &[String]) -> std::process::Output {
    Command::new(task_bin())
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("task runs")
}

/// A fixture copied somewhere Task may write beside it.
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn the_task_is_the_pinned_one() {
    let out = Command::new(task_bin())
        .arg("--version")
        .output()
        .expect("task runs");
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.contains(taste_core::tasks::IDE_TASK_VERSION),
        "the reader is held to Task {}, and this is {said}",
        taste_core::tasks::IDE_TASK_VERSION
    );
}

/// The tasks of the fixture copied to `dir`, as the pinned Task lists
/// them, and the reader's answer for each, read as the IDE reads them: the
/// files the listing names, and the root.
fn listed_and_read(dir: &Path) -> (Vec<Site>, BTreeMap<String, Found>) {
    let listed = task(dir, &["--list-all".into(), "--json".into()]);
    assert!(
        listed.status.success(),
        "{}: {}",
        dir.display(),
        String::from_utf8_lossy(&listed.stderr)
    );
    let listed: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    let root = listed["location"].as_str().unwrap().to_string();
    let sites: Vec<Site> = listed["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| Site {
            name: task["name"].as_str().unwrap().into(),
            taskfile: task["location"]["taskfile"].as_str().unwrap().into(),
            line: task["location"]["line"].as_u64().unwrap() as usize,
            column: task["location"]["column"].as_u64().unwrap() as usize,
        })
        .collect();
    let mut files = BTreeMap::new();
    for path in sites
        .iter()
        .map(|s| s.taskfile.clone())
        .chain([root.clone()])
    {
        let text = std::fs::read_to_string(&path).unwrap();
        files.insert(path, text);
    }
    let answers = taste_taskfile::answer(&Request {
        root,
        files,
        tasks: sites.clone(),
    });
    (sites, answers)
}

#[test]
fn every_fixture_reads_as_task_runs_it() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let scratch = tempfile::tempdir().unwrap();
    let mut differ = Vec::new();
    let mut checked = 0;
    for fixture in std::fs::read_dir(&fixtures).unwrap() {
        let fixture = fixture.unwrap().path();
        let dir = scratch.path().join(fixture.file_name().unwrap());
        copy_dir(&fixture, &dir);
        let (sites, answers) = listed_and_read(&dir);

        for site in &sites {
            let name = format!(
                "{}: {}",
                fixture.file_name().unwrap().to_string_lossy(),
                site.name
            );
            let vars = match answers.get(&site.name) {
                Some(Found::Read(task)) => &task.vars,
                Some(Found::Unknown(why)) => {
                    differ.push(format!("{name}: not read: {why}"));
                    continue;
                }
                None => {
                    differ.push(format!("{name}: no answer"));
                    continue;
                }
            };
            checked += 1;
            let value = |allowed: &Allowed| match allowed {
                Allowed::OneOf(values) => values.first().cloned().unwrap_or_default(),
                Allowed::Any | Allowed::Unresolved => "x".to_string(),
            };
            let given: Vec<(String, String)> = vars
                .iter()
                .map(|v| (v.name.clone(), value(&v.allowed)))
                .collect();
            let mut expect = |what: &str, given: &[(String, String)], code: i32| {
                let mut args = vec!["--dry".to_string(), site.name.clone()];
                args.extend(given.iter().map(|(k, v)| format!("{k}={v}")));
                let out = task(&dir, &args);
                if out.status.code() != Some(code) {
                    differ.push(format!(
                        "{name}: {what}: Task exited {:?}, not {code} ({})",
                        out.status.code(),
                        String::from_utf8_lossy(&out.stderr).trim()
                    ));
                }
            };
            // Everything said to be required, given: nothing else is.
            expect("all given", &given, 0);
            for (i, var) in vars.iter().enumerate() {
                // Each one left out: it is required.
                let mut without = given.clone();
                without.remove(i);
                expect(&format!("{} left out", var.name), &without, 206);
                let with = |v: &str| {
                    let mut with = given.clone();
                    with[i].1 = v.to_string();
                    with
                };
                match &var.allowed {
                    // Each value said to be allowed is, and another is not.
                    Allowed::OneOf(values) => {
                        for v in values {
                            expect(&format!("{}={v}", var.name), &with(v), 0);
                        }
                        expect(&format!("{}={REFUSED}", var.name), &with(REFUSED), 207);
                    }
                    // Anything is: there is no enum it missed.
                    Allowed::Any => expect(&format!("{}={REFUSED}", var.name), &with(REFUSED), 0),
                    Allowed::Unresolved => {}
                }
            }
        }
    }
    assert!(checked > 0, "no fixtures under {}", fixtures.display());
    assert!(
        differ.is_empty(),
        "the reader and Task differ:\n{}",
        differ.join("\n")
    );
}

/// Each task of `fixtures/ports` the reader says serves a port is given
/// that `PORT` when Task runs it; and the reader says so of exactly the
/// tasks whose own `env` sets one — not of a task the file's `env` reaches,
/// nor of one whose `PORT` is a template.
#[test]
fn every_port_is_the_one_task_gives() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ports");
    let scratch = tempfile::tempdir().unwrap();
    let dir = scratch.path().join("ports");
    copy_dir(&fixture, &dir);
    let (sites, answers) = listed_and_read(&dir);
    let mut ports = BTreeMap::new();
    for site in &sites {
        match &answers[&site.name] {
            Found::Read(read) => {
                ports.insert(site.name.clone(), read.port);
            }
            Found::Unknown(why) => panic!("{}: not read: {why}", site.name),
        }
    }
    assert_eq!(
        ports,
        BTreeMap::from([
            ("build".to_string(), None),
            ("docs".to_string(), Some(8808)),
            ("server".to_string(), Some(3000)),
            ("templated".to_string(), None),
            ("web:serve".to_string(), Some(7000)),
        ])
    );
    for (name, port) in &ports {
        let Some(port) = port else { continue };
        let out = task(&dir, &["--silent".into(), name.clone()]);
        assert!(
            out.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            port.to_string(),
            "{name}: Task gives its commands another PORT"
        );
    }
}
