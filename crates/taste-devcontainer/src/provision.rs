//! **VM provisioners: where virtual machines come from.**
//!
//! A provisioner makes a VM and hands back a podman connection; everything
//! downstream — lifecycle, builds, the environment channel, `ide_exec`,
//! relocation — takes that connection and is otherwise unchanged
//! (docs/ENVIRONMENTS.md → "VM provisioners"). This module is the first
//! implementation, a **user-session libvirt** on the machine the IDE is
//! running on, and the two documents every provisioner has to produce
//! whatever it provisions into: what the machine IS, and what the guest
//! does on first boot.
//!
//! # Through `virsh`, not a native binding
//!
//! The same choice podman gets, for the same reasons. `virsh` is a
//! documented public CLI, which is what this project's rules say to use;
//! linking `libvirt` would be a native dependency for the Flatpak to carry
//! into a sandbox that already reaches host tools through
//! `flatpak-spawn --host`; and the shelling-out pattern is the one every
//! other host command here already follows.
//!
//! # `qemu:///session`, never `qemu:///system`
//!
//! Rootless throughout. The session connection needs no root anywhere,
//! `/dev/kvm` is world-accessible on the hosts this targets, and the things
//! the system connection would add — a NAT bridge, PCI passthrough,
//! macvtap — are all things a VM full of untrusted project code should not
//! have. What the session gives up is privileged networking, which is not a
//! loss here: user-mode networking is what egress policy wants anyway,
//! because it puts the whole network stack outside the guest where a
//! compromised guest cannot switch it off.

use std::fmt::Write as _;

use anyhow::{bail, Context, Result};

/// The libvirt connection this project provisions into.
pub const SESSION_URI: &str = "qemu:///session";

/// Everything a domain needs to be defined. Sizing is the caller's: this
/// module builds what it is told to build, so the policy about how much of
/// a host to take lives with whoever is deciding it rather than in the XML
/// writer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainSpec {
    /// libvirt domain name. One per workspace, which is what keeps a
    /// project's blast radius to itself.
    pub name: String,
    pub vcpus: u32,
    pub memory_mib: u64,
    /// The qcow2 this boots — a copy of the pinned guest image
    /// (`crate::guest`), never the pinned file itself, which is a cache
    /// shared by every machine.
    pub disk: String,
    /// The Ignition config, as a file the guest reads once through
    /// `fw_cfg`.
    pub ignition: String,
}

/// The libvirt domain XML for a spec.
///
/// Two things in here are load-bearing and easy to lose:
///
/// - **`<sysinfo type='fwcfg'>`** is how Fedora CoreOS is configured. The
///   guest reads `opt/com.coreos/config` off the firmware config device on
///   first boot; there is no cloud-init and no image customisation step.
/// - **`<interface type='user'>` with the passt backend** puts the network
///   stack in a host process rather than in the guest, which is where the
///   egress policy has to live to be worth anything.
pub fn domain_xml(spec: &DomainSpec) -> Result<String> {
    if spec.name.is_empty() || spec.name.contains(|c: char| c.is_whitespace()) {
        bail!(
            "a domain name may not be empty or contain whitespace: {:?}",
            spec.name
        );
    }
    if spec.vcpus == 0 || spec.memory_mib == 0 {
        bail!("a domain needs at least one vcpu and some memory");
    }
    let mut xml = String::new();
    let name = escape(&spec.name);
    let disk = escape(&spec.disk);
    let ignition = escape(&spec.ignition);
    writeln!(xml, "<domain type='kvm'>")?;
    writeln!(xml, "  <name>{name}</name>")?;
    writeln!(xml, "  <memory unit='MiB'>{}</memory>", spec.memory_mib)?;
    // No ballooning down: qemu never returns page cache it has grown into,
    // so the configured memory IS the commitment. Saying so here rather
    // than discovering it as a ratchet later.
    writeln!(
        xml,
        "  <currentMemory unit='MiB'>{}</currentMemory>",
        spec.memory_mib
    )?;
    writeln!(xml, "  <vcpu placement='static'>{}</vcpu>", spec.vcpus)?;
    writeln!(xml, "  <os>")?;
    writeln!(
        xml,
        "    <type arch='{}' machine='q35'>hvm</type>",
        std::env::consts::ARCH
    )?;
    writeln!(xml, "    <boot dev='hd'/>")?;
    writeln!(xml, "  </os>")?;
    // The Ignition config, handed to the guest through firmware config.
    writeln!(xml, "  <sysinfo type='fwcfg'>")?;
    writeln!(
        xml,
        "    <entry name='opt/com.coreos/config' file='{ignition}'/>"
    )?;
    writeln!(xml, "  </sysinfo>")?;
    writeln!(xml, "  <features>")?;
    writeln!(xml, "    <acpi/>")?;
    writeln!(xml, "    <apic/>")?;
    writeln!(xml, "  </features>")?;
    // Host CPU passthrough, which is also what makes nested virtualisation
    // available inside the guest — the per-build microVM tier wants it.
    writeln!(xml, "  <cpu mode='host-passthrough' check='none'/>")?;
    writeln!(xml, "  <devices>")?;
    writeln!(xml, "    <disk type='file' device='disk'>")?;
    writeln!(xml, "      <driver name='qemu' type='qcow2'/>")?;
    writeln!(xml, "      <source file='{disk}'/>")?;
    writeln!(xml, "      <target dev='vda' bus='virtio'/>")?;
    writeln!(xml, "    </disk>")?;
    writeln!(xml, "    <interface type='user'>")?;
    writeln!(xml, "      <backend type='passt'/>")?;
    writeln!(xml, "      <model type='virtio'/>")?;
    writeln!(xml, "    </interface>")?;
    // A console, so a guest that fails to come up can be read rather than
    // guessed at.
    writeln!(xml, "    <serial type='pty'><target port='0'/></serial>")?;
    writeln!(
        xml,
        "    <console type='pty'><target type='serial' port='0'/></console>"
    )?;
    writeln!(
        xml,
        "    <rng model='virtio'><backend model='random'>/dev/urandom</backend></rng>"
    )?;
    writeln!(xml, "  </devices>")?;
    writeln!(xml, "</domain>")?;
    Ok(xml)
}

/// The networks a guest may not reach: RFC1918 and link-local.
///
/// A random VM on the internet cannot reach the user's router, NAS or
/// printer. A VM on their laptop can, and that is the one place the
/// standard this design is held to leaks (docs/ENVIRONMENTS.md →
/// "Isolation").
pub const PRIVATE_NETWORKS: [&str; 4] = [
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "169.254.0.0/16",
];
/// The IPv6 half of the same.
pub const PRIVATE_NETWORKS_V6: [&str; 2] = ["fc00::/7", "fe80::/10"];

/// What the guest is, on first boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestSpec {
    /// The public half of the key the IDE will reach this VM with. The
    /// private half never leaves the host, and the VM never holds a
    /// credential of the user's — that is the boundary this whole design
    /// defends.
    pub ssh_public_key: String,
    /// The guest's hostname, which is the workspace it serves.
    pub hostname: String,
    /// Refuse traffic to the user's own network.
    ///
    /// **Enforced in the guest, and that limit is worth stating plainly.**
    /// The right place is the host, outside the VM, where a compromised
    /// guest cannot reach it — and there is no way to say it there:
    /// libvirt's passt `<backend>` accepts `type`, `tap`, `vhost`,
    /// `logFile`, `hostname` and `fqdn`, and nothing about egress (read off
    /// libvirt 12.0.0's own schema, not assumed), passt has no CIDR filter,
    /// and a host firewall rule would need root, which this design has
    /// nowhere.
    ///
    /// So it is nftables inside the guest. That stops the threat that
    /// actually exists — project code in a container reaching the LAN,
    /// since containers do not get `NET_ADMIN` and a repo-supplied config
    /// asking for it is refused — and does not stop someone who has become
    /// root in the guest itself. Host-side enforcement is the hardening
    /// this wants next, and it is not done.
    pub deny_private_networks: bool,
}

/// The Ignition config that makes a Fedora CoreOS guest into something the
/// IDE can use.
///
/// Deliberately almost empty. Podman is already in FCOS, and the guest
/// updates itself, so the only things to say are who may log in and what
/// the machine is called. Every additional thing here is a thing that has
/// to keep working across a guest release.
pub fn ignition(spec: &GuestSpec) -> Result<String> {
    if spec.ssh_public_key.trim().is_empty() {
        bail!("a guest with no ssh key is a guest the IDE cannot reach");
    }
    let mut files = vec![serde_json::json!({
        "path": "/etc/hostname",
        "mode": 420,
        "overwrite": true,
        "contents": { "source": data_url(&spec.hostname) },
    })];
    let mut units = vec![serde_json::json!({
        // Podman's API socket, which is what `podman --connection` over ssh
        // ultimately talks to.
        "name": "podman.socket",
        "enabled": true,
    })];
    if spec.deny_private_networks {
        files.push(serde_json::json!({
            "path": "/etc/sysconfig/nftables.conf",
            "mode": 420,
            "overwrite": true,
            "contents": { "source": data_url(&egress_ruleset()) },
        }));
        units.push(serde_json::json!({
            "name": "nftables.service",
            "enabled": true,
        }));
    }
    let config = serde_json::json!({
        "ignition": { "version": "3.4.0" },
        "passwd": {
            "users": [{
                "name": "core",
                "sshAuthorizedKeys": [spec.ssh_public_key.trim()],
            }],
        },
        "storage": { "files": files },
        "systemd": { "units": units },
    });
    serde_json::to_string_pretty(&config).context("serialising the ignition config")
}

/// The ruleset that keeps a guest off the user's network.
///
/// Output filtering, so it covers everything in the guest including every
/// container in it, and written as one set per family so a reader can see
/// at a glance that nothing is missing.
fn egress_ruleset() -> String {
    let v4 = PRIVATE_NETWORKS.join(", ");
    let v6 = PRIVATE_NETWORKS_V6.join(", ");
    format!(
        "#!/usr/sbin/nft -f\n\
         # Written by taste-ide. The internet is allowed; the machine this\n\
         # VM runs on, and everything else on its network, is not.\n\
         table inet taste_egress {{\n\
         \tchain output {{\n\
         \t\ttype filter hook output priority 0; policy accept;\n\
         \t\tip daddr {{ {v4} }} reject\n\
         \t\tip6 daddr {{ {v6} }} reject\n\
         \t}}\n\
         }}\n"
    )
}

/// Ignition takes file contents as a URL; plain text goes inline as a data
/// URL rather than as a second file to place.
fn data_url(text: &str) -> String {
    let encoded: String = text
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect();
    format!("data:,{encoded}%0A")
}

/// XML text escaping. Small and explicit: a domain name or a path with an
/// ampersand in it would otherwise produce XML libvirt rejects, and the
/// failure would be a parse error about a file nobody wrote by hand.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> DomainSpec {
        DomainSpec {
            name: "taste-799fd7acd369bf5c".into(),
            vcpus: 8,
            memory_mib: 10240,
            disk: "/home/dev/.local/share/taste-ide/guests/ws.qcow2".into(),
            ignition: "/home/dev/.local/share/taste-ide/guests/ws.ign".into(),
        }
    }

    /// Prints the generated XML so real libvirt can validate it, which is
    /// the only way to know this module is right — a hand-written
    /// expectation only proves the writer and the test agree. The recipe:
    ///
    /// ```sh
    /// cargo test -p taste-devcontainer --lib provision::tests::dump_xml \
    ///   -- --nocapture | sed -n '/---XML-BEGIN---/,/---XML-END---/p' \
    ///   | sed '1d;$d' > /tmp/domain.xml
    /// virsh -c qemu:///session define /tmp/domain.xml   # then undefine
    /// ```
    ///
    /// libvirt 12.0.0 accepts it and keeps the passt backend, the fwcfg
    /// entry and the CPU mode through its own normalisation, which is what
    /// the three of them being load-bearing means in practice.
    #[test]
    fn dump_xml_for_validation() {
        println!("---XML-BEGIN---");
        print!("{}", domain_xml(&spec()).unwrap());
        println!("---XML-END---");
    }

    /// The two things a CoreOS guest will not come up without.
    #[test]
    fn the_domain_carries_its_ignition_and_a_user_mode_network() {
        let xml = domain_xml(&spec()).unwrap();
        assert!(
            xml.contains("<sysinfo type='fwcfg'>")
                && xml.contains("opt/com.coreos/config")
                && xml.contains("ws.ign"),
            "ignition is delivered through fw_cfg:\n{xml}"
        );
        assert!(
            xml.contains("<interface type='user'>") && xml.contains("<backend type='passt'/>"),
            "the network stack belongs outside the guest:\n{xml}"
        );
        assert!(
            xml.contains("host-passthrough"),
            "nested virt wants it:\n{xml}"
        );
        assert!(xml.contains("<name>taste-799fd7acd369bf5c</name>"));
        assert!(xml.contains("<memory unit='MiB'>10240</memory>"));
        assert!(xml.contains("<vcpu placement='static'>8</vcpu>"));
    }

    /// The LAN denial is NOT in the domain XML, and this test exists so
    /// nobody puts it back. libvirt's passt backend has no egress
    /// vocabulary — checked against its own schema, which allows `type`,
    /// `tap`, `vhost`, `logFile`, `hostname` and `fqdn` and nothing else —
    /// so XML that appeared to deny anything would be decoration.
    #[test]
    fn the_domain_xml_makes_no_claim_about_egress() {
        let xml = domain_xml(&spec()).unwrap();
        for private in ["10.0.0.0/8", "192.168.0.0/16", "fe80::/10"] {
            assert!(
                !xml.contains(private),
                "the domain XML cannot enforce this and must not imply it:\n{xml}"
            );
        }
    }

    #[test]
    fn a_path_with_xml_in_it_does_not_break_the_document() {
        let mut awkward = spec();
        awkward.disk = "/home/dev/a & b/<disk>.qcow2".into();
        let xml = domain_xml(&awkward).unwrap();
        assert!(xml.contains("a &amp; b/&lt;disk&gt;.qcow2"), "{xml}");
        assert!(!xml.contains("<disk>.qcow2"));
    }

    #[test]
    fn a_domain_needs_a_name_and_some_hardware() {
        let mut bad = spec();
        bad.name = String::new();
        assert!(domain_xml(&bad).is_err());
        let mut bad = spec();
        bad.name = "two words".into();
        assert!(domain_xml(&bad).is_err());
        let mut bad = spec();
        bad.vcpus = 0;
        assert!(domain_xml(&bad).is_err());
    }

    /// The guest says who may log in, what it is called, and nothing else
    /// that has to survive a release.
    #[test]
    fn the_ignition_config_is_a_key_a_name_and_podmans_socket() {
        let config = ignition(&GuestSpec {
            ssh_public_key: "ssh-ed25519 AAAAC3Nz taste@host".into(),
            hostname: "taste-sb".into(),
            deny_private_networks: false,
        })
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&config).unwrap();
        assert_eq!(parsed["ignition"]["version"], "3.4.0");
        assert_eq!(parsed["passwd"]["users"][0]["name"], "core");
        assert_eq!(
            parsed["passwd"]["users"][0]["sshAuthorizedKeys"][0],
            "ssh-ed25519 AAAAC3Nz taste@host"
        );
        assert_eq!(parsed["systemd"]["units"][0]["name"], "podman.socket");
        assert_eq!(parsed["systemd"]["units"][0]["enabled"], true);
        let hostname = parsed["storage"]["files"][0]["contents"]["source"]
            .as_str()
            .unwrap();
        assert!(hostname.starts_with("data:,taste-sb"), "{hostname}");
    }

    /// The denial the standard needs, in the one place it can be made.
    #[test]
    fn a_guest_can_be_kept_off_the_users_network() {
        let config = ignition(&GuestSpec {
            ssh_public_key: "ssh-ed25519 AAAAC3Nz taste@host".into(),
            hostname: "taste-sb".into(),
            deny_private_networks: true,
        })
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&config).unwrap();
        let rules = parsed["storage"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == "/etc/sysconfig/nftables.conf")
            .expect("no ruleset was written")["contents"]["source"]
            .as_str()
            .unwrap()
            .to_string();
        // Percent-encoded in the data URL, so match on that spelling.
        for private in ["10.0.0.0%2F8", "192.168.0.0%2F16", "fe80%3A%3A%2F10"] {
            assert!(rules.contains(private), "{private} missing from {rules}");
        }
        let units: Vec<&str> = parsed["systemd"]["units"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u["name"].as_str().unwrap())
            .collect();
        assert!(units.contains(&"nftables.service"), "{units:?}");
    }

    /// A guest not asked to be isolated carries no ruleset at all, rather
    /// than an empty one that reads like a policy.
    #[test]
    fn a_guest_not_asked_to_be_isolated_carries_no_ruleset() {
        let config = ignition(&GuestSpec {
            ssh_public_key: "ssh-ed25519 AAAAC3Nz taste@host".into(),
            hostname: "taste-sb".into(),
            deny_private_networks: false,
        })
        .unwrap();
        assert!(!config.contains("nftables"), "{config}");
    }

    #[test]
    fn a_guest_with_no_key_is_refused() {
        let refused = ignition(&GuestSpec {
            ssh_public_key: "  ".into(),
            hostname: "taste".into(),
            deny_private_networks: true,
        });
        assert!(refused.is_err(), "the IDE could never reach that VM");
    }
}
