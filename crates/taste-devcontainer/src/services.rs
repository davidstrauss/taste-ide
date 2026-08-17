//! systemd inside the devcontainer: unit listing, journal access, and
//! service lifecycle actions.
//!
//! Commands run as container-root through [`ExecContext::resolve_root`] —
//! under rootless podman that is the user's own uid, so nothing here
//! escalates on the host. The blocking functions are meant for
//! `spawn_blocking`; the journal tail runs on the tokio runtime and hands
//! lines over an async channel.

use std::process::Stdio;

use anyhow::{bail, Context, Result};
use taste_core::ExecContext;
use tokio::io::AsyncBufReadExt;

/// One systemd service, as shown in the Services list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceUnit {
    /// Full unit name, e.g. `nginx.service`.
    pub name: String,
    /// systemd ACTIVE column: active / inactive / failed / …
    pub active: String,
    /// systemd SUB column: running / dead / exited / …
    pub sub: String,
    pub description: String,
    /// The `.socket` unit that activates this service, when one exists —
    /// socket activation is the house-preferred way to run services.
    pub socket: Option<String>,
}

impl ServiceUnit {
    pub fn is_running(&self) -> bool {
        self.active == "active"
    }
    pub fn is_failed(&self) -> bool {
        self.active == "failed"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
    Reload,
}

impl ServiceAction {
    pub fn verb(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Reload => "reload",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Start => "Start",
            Self::Stop => "Stop",
            Self::Restart => "Restart",
            Self::Reload => "Reload",
        }
    }
}

/// Unit names come back from systemctl itself, but they also flow into
/// argv for later commands — reject anything outside systemd's own charset
/// as defense in depth.
fn validate_unit(unit: &str) -> Result<()> {
    let ok = !unit.is_empty()
        && !unit.starts_with('-')
        && unit.len() < 256
        && unit
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.@:\\".contains(c));
    if !ok {
        bail!("not a valid unit name: {unit:?}");
    }
    Ok(())
}

fn run_output(exec: &ExecContext, program: &str, args: &[&str]) -> Result<String> {
    let spec = exec.resolve_root(program, args, false);
    let out = std::process::Command::new(&spec.program)
        .args(&spec.args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "{program} {}: {}",
            args.first().unwrap_or(&""),
            stderr.trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// List services (with socket-activation detection). Blocking.
pub fn list_services(exec: &ExecContext) -> Result<Vec<ServiceUnit>> {
    let json = run_output(
        exec,
        "systemctl",
        &[
            "list-units",
            "--type=service,socket",
            "--all",
            "--output=json",
            "--no-pager",
        ],
    )?;
    parse_units(&json)
}

/// Parse `systemctl list-units --output=json` covering services and
/// sockets; sockets fold into their service's `socket` field. Failed units
/// sort first (they need attention), then running, then the rest.
pub fn parse_units(json: &str) -> Result<Vec<ServiceUnit>> {
    #[derive(serde::Deserialize)]
    struct Raw {
        unit: String,
        #[serde(default)]
        active: String,
        #[serde(default)]
        sub: String,
        #[serde(default)]
        description: String,
    }
    let raw: Vec<Raw> = serde_json::from_str(json).context("parsing systemctl JSON")?;
    let sockets: std::collections::HashSet<&str> = raw
        .iter()
        .filter_map(|r| r.unit.strip_suffix(".socket"))
        .collect();
    let mut units: Vec<ServiceUnit> = raw
        .iter()
        .filter(|r| r.unit.ends_with(".service"))
        .map(|r| {
            let stem = r.unit.strip_suffix(".service").unwrap_or(&r.unit);
            ServiceUnit {
                name: r.unit.clone(),
                active: r.active.clone(),
                sub: r.sub.clone(),
                description: r.description.clone(),
                socket: sockets.contains(stem).then(|| format!("{stem}.socket")),
            }
        })
        .collect();
    units.sort_by(|a, b| {
        (!a.is_failed(), !a.is_running(), &a.name).cmp(&(!b.is_failed(), !b.is_running(), &b.name))
    });
    Ok(units)
}

/// Run a lifecycle action on a unit. Blocking; systemctl itself waits for
/// the job, so failures come back with the real error text.
pub fn service_action(exec: &ExecContext, action: ServiceAction, unit: &str) -> Result<()> {
    validate_unit(unit)?;
    run_output(exec, "systemctl", &[action.verb(), unit])?;
    Ok(())
}

/// A bounded read of the journal — the whole system's when `unit` is None.
pub fn journal_snapshot(exec: &ExecContext, unit: Option<&str>, lines: u32) -> Result<String> {
    let n = lines.to_string();
    let mut args = vec!["-n", &n, "--no-pager"];
    if let Some(unit) = unit {
        validate_unit(unit)?;
        args.push("-u");
        args.push(unit);
    }
    run_output(exec, "journalctl", &args)
}

/// One unit file on the container's filesystem, with its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitFile {
    /// The unit this file belongs to (`foo.service` or `foo.socket`).
    pub unit: String,
    /// Absolute path inside the container.
    pub path: String,
    pub content: String,
}

/// Parse `systemctl show -p A,B,…` key=value output.
pub fn parse_show(output: &str) -> std::collections::BTreeMap<String, String> {
    output
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.trim().to_string()))
        .collect()
}

/// The unit files behind a service: its fragment, its drop-ins, and — when
/// the service is socket-activated — the socket unit's files too. Blocking.
pub fn unit_files(exec: &ExecContext, unit: &str) -> Result<Vec<UnitFile>> {
    validate_unit(unit)?;
    let mut queue = vec![unit.to_string()];
    let show = run_output(
        exec,
        "systemctl",
        &["show", "-p", "TriggeredBy", "--no-pager", unit],
    )?;
    if let Some(triggered) = parse_show(&show).get("TriggeredBy") {
        queue.extend(
            triggered
                .split_whitespace()
                .filter(|t| t.ends_with(".socket"))
                .map(str::to_string),
        );
    }
    let mut files = Vec::new();
    for u in queue {
        validate_unit(&u)?;
        let show = run_output(
            exec,
            "systemctl",
            &["show", "-p", "FragmentPath,DropInPaths", "--no-pager", &u],
        )?;
        let props = parse_show(&show);
        let mut paths: Vec<String> = Vec::new();
        if let Some(p) = props.get("FragmentPath") {
            if !p.is_empty() {
                paths.push(p.clone());
            }
        }
        if let Some(dropins) = props.get("DropInPaths") {
            paths.extend(dropins.split_whitespace().map(str::to_string));
        }
        for path in paths {
            let content =
                run_output(exec, "cat", &[&path]).unwrap_or_else(|e| format!("(unreadable: {e})"));
            files.push(UnitFile {
                unit: u.clone(),
                path,
                content,
            });
        }
    }
    Ok(files)
}

/// A live `journalctl -f`, killed on [`JournalTail::stop`] or drop.
pub struct JournalTail {
    pub lines: async_channel::Receiver<String>,
    stop: async_channel::Sender<()>,
}

impl JournalTail {
    pub fn stop(&self) {
        self.stop.close();
    }
}

/// Start following the journal on the tokio runtime. Lines arrive on the
/// returned channel; dropping the handle (or calling `stop`) kills the
/// underlying process.
pub fn tail_journal(
    handle: &tokio::runtime::Handle,
    exec: &ExecContext,
    unit: Option<String>,
    initial_lines: u32,
) -> JournalTail {
    let n = initial_lines.to_string();
    let mut args: Vec<&str> = vec!["-f", "-n", &n, "--no-pager"];
    if let Some(unit) = unit.as_deref() {
        args.push("-u");
        args.push(unit);
    }
    let spec = exec.resolve_root("journalctl", &args, false);
    let (line_tx, line_rx) = async_channel::bounded(512);
    let (stop_tx, stop_rx) = async_channel::bounded::<()>(1);
    handle.spawn(async move {
        let mut child = match tokio::process::Command::new(&spec.program)
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                let _ = line_tx.send(format!("journal tail failed: {e}")).await;
                return;
            }
        };
        let stdout = child.stdout.take().expect("stdout piped");
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        loop {
            tokio::select! {
                // Ok or Closed both mean stop.
                _ = stop_rx.recv() => break,
                line = lines.next_line() => match line {
                    Ok(Some(line)) => {
                        if line_tx.send(line).await.is_err() {
                            break;
                        }
                    }
                    _ => break,
                },
            }
        }
        let _ = child.start_kill();
    });
    JournalTail {
        lines: line_rx,
        stop: stop_tx,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[
        {"unit":"zed.service","load":"loaded","active":"active","sub":"running","description":"Z"},
        {"unit":"broken.service","load":"loaded","active":"failed","sub":"failed","description":"B"},
        {"unit":"idle.service","load":"loaded","active":"inactive","sub":"dead","description":"I"},
        {"unit":"web.service","load":"loaded","active":"active","sub":"running","description":"W"},
        {"unit":"web.socket","load":"loaded","active":"active","sub":"listening","description":"W sock"}
    ]"#;

    #[test]
    fn parses_sorts_and_pairs_sockets() {
        let units = parse_units(SAMPLE).unwrap();
        let names: Vec<&str> = units.iter().map(|u| u.name.as_str()).collect();
        // Failed first, then running (alphabetical), then the rest; the
        // socket itself is folded into its service, not listed.
        assert_eq!(
            names,
            vec![
                "broken.service",
                "web.service",
                "zed.service",
                "idle.service"
            ]
        );
        let web = units.iter().find(|u| u.name == "web.service").unwrap();
        assert_eq!(web.socket.as_deref(), Some("web.socket"));
        let zed = units.iter().find(|u| u.name == "zed.service").unwrap();
        assert_eq!(zed.socket, None);
    }

    #[test]
    fn action_verbs() {
        assert_eq!(ServiceAction::Start.verb(), "start");
        assert_eq!(ServiceAction::Reload.verb(), "reload");
        assert_eq!(ServiceAction::Stop.label(), "Stop");
    }

    #[test]
    fn unit_validation_rejects_argv_injection() {
        assert!(validate_unit("nginx.service").is_ok());
        assert!(validate_unit("dbus-:1.2-org.service").is_ok());
        assert!(validate_unit("foo; rm -rf /").is_err());
        assert!(validate_unit("--flag").is_err()); // would parse as an option
        assert!(validate_unit("").is_err());
    }

    #[test]
    fn parses_show_output() {
        let props = parse_show("FragmentPath=/etc/systemd/system/web.service\nDropInPaths=/a.conf /b.conf\nTriggeredBy=web.socket\n");
        assert_eq!(props["FragmentPath"], "/etc/systemd/system/web.service");
        assert_eq!(props["DropInPaths"], "/a.conf /b.conf");
        assert_eq!(props["TriggeredBy"], "web.socket");
    }
}
