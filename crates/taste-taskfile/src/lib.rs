//! What each task of a Taskfile says beyond its name: the variables its
//! `requires` names that nothing else gives a value, and the values each
//! may take — for the Tasks section's play, which asks for them, and the
//! file tree's menus, which offer a task that takes `FILE` on a file, one
//! that takes `DIR` on a folder, and one that takes `FILE_OR_DIR` on both
//! — and the port it serves, which pairs it with that forwarded port.
//!
//! **Read from the Taskfile, held to Task.** `task --list-all --json`
//! names every task, the file it is in, and where, but says nothing of
//! `requires` or `env`. The Taskfile is a documented format with a
//! published schema, so it is read here directly (CLAUDE.md → intended
//! interfaces); what the reading must agree with is Task's own behavior,
//! and `tests/against_task.rs` runs the pinned Task on the same fixtures
//! and compares: `--dry`, variable by variable, and each task said to
//! serve a port, run, for the `PORT` its commands are given.
//!
//! **A task's port is its own `env`'s `PORT`**, a literal number: the
//! twelve-factor convention, which Rails, most Node frameworks, and many
//! Python servers read themselves, so the pairing is the setting the
//! server runs with rather than a note beside it that can drift from it,
//! and any tool that reads the Taskfile reads it the same way — VS Code's
//! as well as this IDE's (David, 2026-11-13, of the
//! `customizations.taste-ide.services` block it replaces: "encode the
//! port:task mapping without having it be custom data specific to Taste
//! IDE"). A `PORT` the Taskfile's own `env` gives every task is not one:
//! it would make every task a server. A template or a `sh:` is not one
//! either, since only Task knows its value.
//!
//! **What counts as given**, as Task counts it: a value from the task's own
//! `vars`, from its Taskfile's `vars`, from the `vars` of every include on
//! the way to it from the root, or from a Taskfile above it on that way.
//! An empty value is a value. Two sources are NOT seen here: the
//! environment Task runs in (`HOME` satisfies a `requires: [HOME]`) and
//! `dotenv` files. A variable either supplies is asked for anyway, and a
//! caller lets the answer be left empty, so Task falls back to it.
//!
//! **An `enum` Task fills in at run time** (`enum: {ref: .MODES}`) is
//! [`Allowed::Unresolved`]: any value can be typed, and Task refuses one it
//! does not allow.
//!
//! Paths are as Task reported them — the container's — and no file is read
//! here: the caller hands over the text of each, and an include whose file
//! it did not hand over, or whose path is a template, is not followed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value};

/// The names Task looks for a Taskfile under in a directory, in its order
/// (`taste_core::conventions::TASKFILE_NAMES`, which this crate does not
/// link).
const TASKFILE_NAMES: [&str; 8] = [
    "Taskfile.yml",
    "taskfile.yml",
    "Taskfile.yaml",
    "taskfile.yaml",
    "Taskfile.dist.yml",
    "taskfile.dist.yml",
    "Taskfile.dist.yaml",
    "taskfile.dist.yaml",
];

/// How deep includes are followed.
const MAX_DEPTH: usize = 16;

/// What the helper is asked.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Request {
    /// The root Taskfile, as `task --list-all --json`'s `location` names it.
    pub root: String,
    /// Each Taskfile's text, by its path.
    pub files: BTreeMap<String, String>,
    /// The tasks, as the listing gives them.
    pub tasks: Vec<Site>,
}

/// One task as `task --list-all --json` lists it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Site {
    /// Its full name, namespaces and all (`rs:fmt`).
    pub name: String,
    /// The file it is defined in.
    pub taskfile: String,
    /// Where its key is, 1-based.
    pub line: usize,
    pub column: usize,
}

/// The values a required variable may take.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Allowed {
    /// Anything.
    Any,
    /// One of these (`enum: [check, write]`).
    OneOf(Vec<String>),
    /// An `enum` Task fills in from a variable at run time.
    Unresolved,
}

/// One variable a task needs given.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Required {
    pub name: String,
    pub allowed: Allowed,
}

/// What was read of one task.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Task {
    /// The variables it needs given; empty when it needs none.
    pub vars: Vec<Required>,
    /// The port it serves: its own `env`'s literal `PORT`.
    pub port: Option<u16>,
}

/// What was found for one task.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Found {
    /// What it says.
    Read(Task),
    /// Why it could not be read.
    Unknown(String),
}

/// Where the tasks of one Taskfile are reached from the root: the
/// namespace their names carry, and the variables given above them.
struct Scope {
    file: String,
    prefix: String,
    given: BTreeSet<String>,
}

/// Every task's answer, by name.
pub fn answer(request: &Request) -> BTreeMap<String, Found> {
    let parsed: BTreeMap<&str, Result<Value, String>> = request
        .files
        .iter()
        .map(|(path, text)| (path.as_str(), parse(text)))
        .collect();
    let mut scopes = Vec::new();
    walk(
        &parsed,
        &request.root,
        String::new(),
        BTreeSet::new(),
        0,
        &mut Vec::new(),
        &mut scopes,
    );
    request
        .tasks
        .iter()
        .map(|site| {
            (
                site.name.clone(),
                find(&parsed, &request.files, &scopes, site),
            )
        })
        .collect()
}

fn parse(text: &str) -> Result<Value, String> {
    let mut value: Value = serde_yaml_ng::from_str(text).map_err(|e| e.to_string())?;
    value.apply_merge().map_err(|e| e.to_string())?;
    Ok(value)
}

/// The scopes of `file` and of everything it includes.
fn walk(
    parsed: &BTreeMap<&str, Result<Value, String>>,
    file: &str,
    prefix: String,
    mut given: BTreeSet<String>,
    depth: usize,
    path: &mut Vec<String>,
    scopes: &mut Vec<Scope>,
) {
    if depth > MAX_DEPTH || path.iter().any(|seen| seen == file) {
        return;
    }
    let Some(Ok(doc)) = parsed.get(file) else {
        return;
    };
    given.extend(keys(doc.get("vars")));
    scopes.push(Scope {
        file: file.to_string(),
        prefix: prefix.clone(),
        given: given.clone(),
    });
    let Some(includes) = doc.get("includes").and_then(Value::as_mapping) else {
        return;
    };
    path.push(file.to_string());
    for (namespace, include) in includes {
        let Some(namespace) = scalar(namespace) else {
            continue;
        };
        let (target, vars, flatten) = match include {
            Value::String(target) => (target.as_str(), None, false),
            Value::Mapping(include) => {
                let Some(target) = include.get("taskfile").and_then(Value::as_str) else {
                    continue;
                };
                let flatten = include
                    .get("flatten")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                (target, include.get("vars"), flatten)
            }
            _ => continue,
        };
        if target.contains("{{") {
            continue;
        }
        let Some(included) = resolve(parsed, file, target) else {
            continue;
        };
        let mut inner = given.clone();
        inner.extend(keys(vars));
        let prefix = match (flatten, prefix.is_empty()) {
            (true, _) => prefix.clone(),
            (false, true) => namespace,
            (false, false) => format!("{prefix}:{namespace}"),
        };
        walk(parsed, &included, prefix, inner, depth + 1, path, scopes);
    }
    path.pop();
}

/// The file an include's `taskfile` names, relative to the file including
/// it: the file itself, or a Taskfile in the directory it names.
fn resolve(
    parsed: &BTreeMap<&str, Result<Value, String>>,
    from: &str,
    target: &str,
) -> Option<String> {
    let base = Path::new(from).parent().unwrap_or(Path::new("/"));
    let joined = normalize(&base.join(target));
    std::iter::once(joined.clone())
        .chain(TASKFILE_NAMES.iter().map(|name| joined.join(name)))
        .map(|path| path.to_string_lossy().into_owned())
        .find(|path| parsed.contains_key(path.as_str()))
}

/// `path` with its `.` and `..` taken out, as a path is compared by name.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The answer for one task.
fn find(
    parsed: &BTreeMap<&str, Result<Value, String>>,
    texts: &BTreeMap<String, String>,
    scopes: &[Scope],
    site: &Site,
) -> Found {
    let doc = match parsed.get(site.taskfile.as_str()) {
        Some(Ok(doc)) => doc,
        Some(Err(why)) => return Found::Unknown(format!("{} is not YAML: {why}", site.taskfile)),
        None => return Found::Unknown(format!("{} was not read", site.taskfile)),
    };
    let Some(tasks) = doc.get("tasks").and_then(Value::as_mapping) else {
        return Found::Unknown(format!("{} has no tasks", site.taskfile));
    };
    // Each way the name could be this file's key: a scope of this file
    // whose namespace starts it.
    let mut candidates: Vec<(&str, &Scope)> = scopes
        .iter()
        .filter(|scope| scope.file == site.taskfile)
        .filter_map(|scope| {
            let local = if scope.prefix.is_empty() {
                site.name.as_str()
            } else {
                site.name.strip_prefix(&scope.prefix)?.strip_prefix(':')?
            };
            tasks.contains_key(local).then_some((local, scope))
        })
        .collect();
    // Two (a namespace that is a prefix of another's): the key at the
    // place the listing named.
    if candidates.len() > 1 {
        let line = texts
            .get(&site.taskfile)
            .and_then(|text| text.lines().nth(site.line.saturating_sub(1)))
            .unwrap_or("");
        let at: String = line.chars().skip(site.column.saturating_sub(1)).collect();
        candidates.retain(|(local, _)| key_at(&at, local));
    }
    let Some(&(local, scope)) = candidates.first() else {
        return Found::Unknown(format!(
            "{} was not found in {} as it is included",
            site.name, site.taskfile
        ));
    };
    let task = &tasks[local];
    let mut given = scope.given.clone();
    given.extend(keys(task.get("vars")));
    let declared = task
        .get("requires")
        .and_then(|requires| requires.get("vars"))
        .and_then(Value::as_sequence);
    let mut vars = Vec::new();
    for var in declared.into_iter().flatten() {
        let (name, allowed) = match var {
            Value::Mapping(var) => {
                let Some(name) = var.get("name").and_then(scalar) else {
                    continue;
                };
                let allowed = match var.get("enum") {
                    None | Some(Value::Null) => Allowed::Any,
                    Some(Value::Sequence(values)) => {
                        Allowed::OneOf(values.iter().filter_map(scalar).collect())
                    }
                    Some(_) => Allowed::Unresolved,
                };
                (name, allowed)
            }
            other => match scalar(other) {
                Some(name) => (name, Allowed::Any),
                None => continue,
            },
        };
        if !given.contains(&name) && !vars.iter().any(|v: &Required| v.name == name) {
            vars.push(Required { name, allowed });
        }
    }
    Found::Read(Task {
        vars,
        port: port(task),
    })
}

/// The port a task serves: its own `env`'s `PORT`, when that is a number
/// from 1 to 65535, written as one or as a string of one.
fn port(task: &Value) -> Option<u16> {
    let value = task.get("env")?.get("PORT")?;
    let number = match value {
        Value::Number(n) => n.as_u64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    u16::try_from(number).ok().filter(|port| *port > 0)
}

/// Whether `at` — a line from a key's column on — starts with `key`, bare
/// or quoted, and its colon.
fn key_at(at: &str, key: &str) -> bool {
    let rest = [
        at.strip_prefix(key),
        at.strip_prefix('"')
            .and_then(|at| at.strip_prefix(key))
            .and_then(|at| at.strip_prefix('"')),
        at.strip_prefix('\'')
            .and_then(|at| at.strip_prefix(key))
            .and_then(|at| at.strip_prefix('\'')),
    ];
    rest.into_iter()
        .flatten()
        .any(|rest| rest.trim_start().starts_with(':'))
}

/// The names a `vars` mapping gives.
fn keys(vars: Option<&Value>) -> impl Iterator<Item = String> + '_ {
    vars.and_then(Value::as_mapping)
        .into_iter()
        .flat_map(Mapping::keys)
        .filter_map(scalar)
}

/// A scalar as the string Task reads it as.
fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(files: &[(&str, &str)], tasks: &[(&str, &str, usize)]) -> BTreeMap<String, Found> {
        answer(&Request {
            root: files[0].0.to_string(),
            files: files
                .iter()
                .map(|(path, text)| (path.to_string(), text.to_string()))
                .collect(),
            tasks: tasks
                .iter()
                .map(|(name, file, line)| Site {
                    name: name.to_string(),
                    taskfile: file.to_string(),
                    line: *line,
                    column: 3,
                })
                .collect(),
        })
    }

    #[test]
    fn a_short_and_a_long_requirement_with_their_values() {
        let found = ask(
            &[(
                "/w/Taskfile.yml",
                "version: '3'\ntasks:\n  fmt:\n    requires:\n      vars:\n        - FILE\n        \
                 - name: MODE\n          enum: [check, write]\n",
            )],
            &[("fmt", "/w/Taskfile.yml", 3)],
        );
        assert_eq!(
            found["fmt"],
            Found::Read(Task {
                vars: vec![
                    Required {
                        name: "FILE".into(),
                        allowed: Allowed::Any
                    },
                    Required {
                        name: "MODE".into(),
                        allowed: Allowed::OneOf(vec!["check".into(), "write".into()])
                    },
                ],
                port: None,
            })
        );
    }

    #[test]
    fn an_include_names_its_tasks_and_gives_its_vars() {
        let found = ask(
            &[
                (
                    "/w/Taskfile.yml",
                    "version: '3'\nincludes:\n  rs:\n    taskfile: ./sub\n    vars: {MODE: x}\n",
                ),
                (
                    "/w/sub/Taskfile.yml",
                    "version: '3'\ntasks:\n  fmt:\n    requires: {vars: [FILE, MODE]}\n",
                ),
            ],
            &[("rs:fmt", "/w/sub/Taskfile.yml", 3)],
        );
        assert_eq!(
            found["rs:fmt"],
            Found::Read(Task {
                vars: vec![Required {
                    name: "FILE".into(),
                    allowed: Allowed::Any
                }],
                port: None,
            })
        );
    }

    #[test]
    fn a_file_not_handed_over_is_unknown_rather_than_guessed() {
        let found = ask(
            &[("/w/Taskfile.yml", "version: '3'\n")],
            &[("rs:fmt", "/w/sub/Taskfile.yml", 3)],
        );
        assert!(matches!(found["rs:fmt"], Found::Unknown(_)));
    }

    /// A task's port is its own `env`'s `PORT`, a number or a string of
    /// one; the file's `env`, a template, a `sh:`, and a number no port
    /// can be are not one.
    #[test]
    fn a_task_serves_the_port_its_own_env_gives() {
        let text = "version: '3'\nenv:\n  PORT: 9999\ntasks:\n  \
                    a:\n    env: {PORT: 3000}\n  \
                    b:\n    env: {PORT: '8808'}\n  \
                    c:\n    cmds: [true]\n  \
                    d:\n    env: {PORT: '{{.P}}'}\n  \
                    e:\n    env: {PORT: {sh: echo 1}}\n  \
                    f:\n    env: {PORT: 70000}\n  \
                    g:\n    env: {PORT: 0}\n";
        let names = ["a", "b", "c", "d", "e", "f", "g"];
        let sites: Vec<(&str, &str, usize)> = names
            .iter()
            .map(|name| (*name, "/w/Taskfile.yml", 1))
            .collect();
        let found = ask(&[("/w/Taskfile.yml", text)], &sites);
        let port = |name: &str| match &found[name] {
            Found::Read(task) => task.port,
            Found::Unknown(why) => panic!("{name}: {why}"),
        };
        assert_eq!(port("a"), Some(3000));
        assert_eq!(port("b"), Some(8808));
        for name in ["c", "d", "e", "f", "g"] {
            assert_eq!(port(name), None, "{name}");
        }
    }

    #[test]
    fn a_key_is_found_bare_or_quoted() {
        assert!(key_at("fmt:", "fmt"));
        assert!(key_at("\"file:fmt\" :", "file:fmt"));
        assert!(key_at("'a': {}", "a"));
        assert!(!key_at("fmtx:", "fmt"));
    }
}
