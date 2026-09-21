//! **Bytes through a forwarded port, counted where the port is.**
//!
//! A container's published port lives on the VM's loopback, and every
//! packet to or from it crosses the guest's netfilter hooks — TCP and
//! UDP alike, which is what the kernel's per-socket counters never gave
//! (docs/spikes/header-budget-glyphs-and-port-traffic.md § 3 found port
//! traffic TCP-only for that reason). The guest is the IDE's, root and
//! all, so the IDE keeps an nftables table there, `inet taste_ports`, with
//! a pair of chains per environment — `in_<env>` on the input hook
//! counting bytes to each published port, `out_<env>` on the output hook
//! counting bytes from it — and reads the counters back every five
//! seconds over ssh (David, 2026-09-21: "we ought to be able to show
//! proper ingress/egress sparklines for ports, even for UDP"). The rules
//! accept everything; they only count.

use anyhow::{Context, Result};
use taste_core::activity::{Count, BUCKETS};
use taste_core::environment::EnvironmentId;

use crate::keys::Keys;
use crate::provision::Vm;

/// The table the counters live in.
pub const TABLE: &str = "taste_ports";

/// An environment's chain names: nft identifiers, so the id's dashes
/// become underscores.
pub fn chains(env: &EnvironmentId) -> (String, String) {
    let safe: String = env
        .as_str()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    (format!("in_{safe}"), format!("out_{safe}"))
}

/// The environment `chain` counts for, when it is one of ours.
pub fn env_of_chain(chain: &str) -> Option<(&str, bool)> {
    if let Some(env) = chain.strip_prefix("in_") {
        Some((env, true))
    } else {
        chain.strip_prefix("out_").map(|env| (env, false))
    }
}

/// The `nft -f` script that makes `env`'s counters count exactly
/// `forwards` — the published host ports — replacing whatever they
/// counted before. Idempotent: `add table` and `add chain` are no-ops on
/// what exists, and the flush empties the chains before the rules go in.
pub fn install_script(env: &EnvironmentId, forwards: &[u16]) -> String {
    let (input, output) = chains(env);
    let mut script = format!(
        "add table inet {TABLE}\n\
         add chain inet {TABLE} {input} {{ type filter hook input priority 0; policy accept; }}\n\
         add chain inet {TABLE} {output} {{ type filter hook output priority 0; policy accept; }}\n\
         flush chain inet {TABLE} {input}\n\
         flush chain inet {TABLE} {output}\n"
    );
    for port in forwards {
        script.push_str(&format!(
            "add rule inet {TABLE} {input} meta l4proto {{ tcp, udp }} th dport {port} counter comment \"{port}\"\n\
             add rule inet {TABLE} {output} meta l4proto {{ tcp, udp }} th sport {port} counter comment \"{port}\"\n"
        ));
    }
    script
}

/// The script that takes `env`'s counters away — a container stopped.
pub fn remove_script(env: &EnvironmentId) -> String {
    let (input, output) = chains(env);
    format!(
        "add table inet {TABLE}\n\
         add chain inet {TABLE} {input}\n\
         add chain inet {TABLE} {output}\n\
         delete chain inet {TABLE} {input}\n\
         delete chain inet {TABLE} {output}\n"
    )
}

/// One counting rule as `nft -j list table` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counter {
    pub chain: String,
    /// The port, as the rule's comment.
    pub port: u16,
    pub bytes: u64,
}

/// The counters out of `nft -j list table inet taste_ports`: every rule
/// with a comment and a counter, in the order listed.
pub fn parse_counters(json: &str) -> Result<Vec<Counter>> {
    let value: serde_json::Value = serde_json::from_str(json).context("reading nft's JSON")?;
    let mut counters = Vec::new();
    for item in value["nftables"].as_array().into_iter().flatten() {
        let Some(rule) = item.get("rule") else {
            continue;
        };
        let (Some(chain), Some(comment)) = (rule["chain"].as_str(), rule["comment"].as_str())
        else {
            continue;
        };
        let Ok(port) = comment.parse::<u16>() else {
            continue;
        };
        let bytes = rule["expr"]
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|expr| expr.get("counter").and_then(|c| c["bytes"].as_u64()));
        if let Some(bytes) = bytes {
            counters.push(Counter {
                chain: chain.to_string(),
                port,
                bytes,
            });
        }
    }
    Ok(counters)
}

/// Run `nft` as root in the VM, with `script` on its stdin when there is
/// one. `core` has passwordless sudo on the guest by its image's
/// contract; `-n` so it never asks.
pub fn nft_in_guest(keys: &Keys, vm: &Vm, args: &[&str], script: Option<&str>) -> Result<String> {
    let mut remote: Vec<String> = vec!["sudo".into(), "-n".into(), "nft".into()];
    remote.extend(args.iter().map(|a| a.to_string()));
    let (program, argv) = keys.ssh_argv(vm.ssh_port, remote);
    let mut command = std::process::Command::new(&program);
    command
        .args(&argv)
        .stdin(if script.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("running nft in VM {}", vm.domain))?;
    if let (Some(script), Some(mut stdin)) = (script, child.stdin.take()) {
        use std::io::Write;
        stdin.write_all(script.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!(
            "nft in VM {}: {}",
            vm.domain,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Make `env`'s counters count `forwards`, or take them away when there
/// are none. Blocking; the supervisor runs it beside the port forward.
pub fn sync_counters(keys: &Keys, vm: &Vm, env: &EnvironmentId, forwards: &[u16]) -> Result<()> {
    let script = if forwards.is_empty() {
        remove_script(env)
    } else {
        install_script(env, forwards)
    };
    nft_in_guest(keys, vm, &["-f", "-"], Some(&script)).map(|_| ())
}

/// Read every environment's counters in `vm`.
pub fn read_counters(keys: &Keys, vm: &Vm) -> Result<Vec<Counter>> {
    let json = nft_in_guest(keys, vm, &["-j", "list", "table", "inet", TABLE], None)?;
    parse_counters(&json)
}

/// Bytes through one published port over the last five minutes, in the
/// sparkline's buckets: into the service and out of it, KiB per second,
/// with the latest readings for the tooltip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortTraffic {
    pub ingress: [Count; BUCKETS],
    pub egress: [Count; BUCKETS],
    pub ingress_now_kib_s: u32,
    pub egress_now_kib_s: u32,
}

impl Default for PortTraffic {
    fn default() -> Self {
        Self {
            ingress: [0; BUCKETS],
            egress: [0; BUCKETS],
            ingress_now_kib_s: 0,
            egress_now_kib_s: 0,
        }
    }
}

impl PortTraffic {
    /// One more reading of each direction.
    pub fn push(&mut self, ingress_kib_s: u32, egress_kib_s: u32) {
        fn shift(ring: &mut [Count], value: u32) {
            ring.rotate_left(1);
            if let Some(last) = ring.last_mut() {
                *last = u16::try_from(value).unwrap_or(u16::MAX);
            }
        }
        shift(&mut self.ingress, ingress_kib_s);
        shift(&mut self.egress, egress_kib_s);
        self.ingress_now_kib_s = ingress_kib_s;
        self.egress_now_kib_s = egress_kib_s;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The script makes the chains, empties them, and counts each port
    /// both ways for TCP and UDP together; removal deletes the chains.
    #[test]
    fn the_scripts_count_each_port_both_ways() {
        let env = EnvironmentId::parse("i-0001").unwrap();
        let script = install_script(&env, &[3000, 5432]);
        assert!(script.contains("add chain inet taste_ports in_i_0001 { type filter hook input"));
        assert!(script.contains("add chain inet taste_ports out_i_0001 { type filter hook output"));
        assert!(script.contains("flush chain inet taste_ports in_i_0001"));
        assert!(script.contains(
            "add rule inet taste_ports in_i_0001 meta l4proto { tcp, udp } th dport 3000 counter comment \"3000\""
        ));
        assert!(script.contains(
            "add rule inet taste_ports out_i_0001 meta l4proto { tcp, udp } th sport 5432 counter comment \"5432\""
        ));
        let gone = remove_script(&env);
        assert!(gone.contains("delete chain inet taste_ports in_i_0001"));
        assert_eq!(env_of_chain("in_i_0001"), Some(("i_0001", true)));
        assert_eq!(env_of_chain("out_primary"), Some(("primary", false)));
        assert_eq!(env_of_chain("egress"), None);
    }

    /// nft's JSON, as the guest printed it for a rule made by the script.
    #[test]
    fn counters_are_read_off_nfts_json() {
        let json = r#"{"nftables": [{"metainfo": {"version": "1.1.6"}}, {"table": {"family": "inet", "name": "taste_ports", "handle": 5}}, {"chain": {"family": "inet", "table": "taste_ports", "name": "in_x", "handle": 1, "type": "filter", "hook": "input", "prio": 0, "policy": "accept"}}, {"rule": {"family": "inet", "table": "taste_ports", "chain": "in_x", "handle": 3, "comment": "3000", "expr": [{"match": {"op": "==", "left": {"meta": {"key": "l4proto"}}, "right": {"set": ["tcp", "udp"]}}}, {"match": {"op": "==", "left": {"payload": {"protocol": "th", "field": "dport"}}, "right": 3000}}, {"counter": {"packets": 12, "bytes": 4096}}]}}]}"#;
        assert_eq!(
            parse_counters(json).unwrap(),
            vec![Counter {
                chain: "in_x".into(),
                port: 3000,
                bytes: 4096,
            }]
        );
    }
}
