//! **VM provisioners: where virtual machines come from.**
//!
//! A provisioner makes a VM and hands back a podman connection; everything
//! downstream — lifecycle, builds, the environment channel, `ide_exec`,
//! relocation — takes that connection and is otherwise unchanged
//! (docs/ENVIRONMENTS.md → "VM provisioners"). This module is the first
//! implementation, a **user-session libvirt** on the machine the IDE is
//! running on: the two documents every provisioner has to produce whatever
//! it provisions into — what the machine IS ([`domain_xml`]) and what the
//! guest does on first boot ([`ignition`]) — and the lifecycle that turns
//! them into a running guest with a registered connection
//! ([`LibvirtSession`]).
//!
//! # A pool per workspace, named with entropy
//!
//! A workspace does not have *a* VM; it has as many as its environments
//! need, placed by capacity, because a few small VMs are easier to obtain
//! than one large one and some capacity does not scale linearly (David,
//! 2026-09-20). What keeps them a pool rather than a crowd is the name:
//! every domain is `taste-<workspace-key>-<slug>`, so [`domain_prefix`]
//! enumerates a workspace's VMs from libvirt itself and nothing has to be
//! remembered about them. The slug is six random characters from the same
//! draw as issue ids — never a counter, which would promise an ordering and
//! a reuse that nothing here keeps.
//!
//! One VM serves one workspace. That is clause 5 of the standard — other
//! projects the user has open — and it is why the pool is per workspace
//! rather than per user.
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
//! loss here: user-mode networking (passt) is what the IDE wants, and the
//! one door through it is the inbound ssh forward the IDE itself asks for.
//!
//! # Rootless podman in the guest, as `core`
//!
//! The guest runs podman rootless as its `core` user (uid 1000) — the shape
//! the substrate spike measured and the one `podman machine` uses — so the
//! supervisor's `--userns=keep-id` mapping and everything else it does with
//! a checkout's ownership are unchanged when the checkout is over there.
//! Ignition arranges it: `core` lingers, and the user `podman.socket` is
//! enabled, which is the socket `podman --connection` over ssh talks to.
//!
//! # The guest is never updated; it is replaced
//!
//! Fedora CoreOS would update itself through zincati, and zincati
//! **reboots** the guest to apply an update — which is every container in
//! it killed mid-work at a time nobody chose. So auto-updates are off, and
//! a guest runs the release it was built from until it is replaced: the
//! pin (`crate::guest`) decides what new VMs boot, and an environment moves
//! to a fresh VM by backup and restore — its snapshot ref, its config, its
//! agent's home volume — rather than the VM changing under it (David,
//! 2026-09-20: "instead of ever updating the VMs, leveraging backup +
//! restore to move envs to new hosts"). A pin that has fallen behind the
//! stream is a fact to surface, not a reboot to schedule.
//!
//! # One fact, one place
//!
//! The ssh port a VM listens on is in the domain XML, as the passt
//! `<portForward>`, and it is read back from `virsh dumpxml` rather than
//! kept in a second file that could disagree. The workspace a VM belongs to
//! is in its `<metadata>`, for the same reason: a domain that libvirt has
//! and the IDE has forgotten is still attributable.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use taste_core::podman::{host_argv, PodmanTarget};

use crate::keys::{HostKey, Keys};
use crate::sizing::Sizing;

/// The libvirt connection this project provisions into.
pub const SESSION_URI: &str = "qemu:///session";

/// The guest's user: Fedora CoreOS's default, uid 1000, and the one that
/// owns every checkout in the VM.
pub const GUEST_USER: &str = "core";
/// Where rootless podman's API socket is for that user, which is the far
/// end of every `podman --connection` the IDE makes.
pub const GUEST_PODMAN_SOCKET: &str = "/run/user/1000/podman/podman.sock";
/// The XML namespace the IDE's own metadata rides under in a domain.
pub const METADATA_NS: &str = "https://taste-ide.dev/xmlns/vm/1";
/// Characters of entropy in a domain name's slug.
pub const NAME_ENTROPY: usize = 6;
/// How long a guest gets from `start` to a podman that answers over ssh.
/// A first boot runs Ignition and grows its root filesystem before sshd is
/// up, and podman's user socket comes after login.
pub const READY_TIMEOUT: Duration = Duration::from_secs(240);

/// `taste-<workspace-key>-` — what every VM of a workspace is named under.
pub fn domain_prefix(workspace_root: &Path) -> String {
    format!(
        "taste-{}-",
        taste_core::environment::workspace_key(workspace_root)
    )
}

/// A fresh domain name for a workspace: its prefix and [`NAME_ENTROPY`]
/// random characters.
pub fn mint_domain_name(workspace_root: &Path) -> Result<String> {
    Ok(format!(
        "{}{}",
        domain_prefix(workspace_root),
        taste_git::random_slug(NAME_ENTROPY)?
    ))
}

/// Where the VMs' disks live: `$XDG_DATA_HOME/taste-ide/guests/machines`,
/// beside the base images they overlay. Data, not state: a disk is derived
/// and disposable, the way every VM is.
pub fn disks_dir() -> PathBuf {
    crate::guest::images_dir().join("machines")
}

/// Everything a domain needs to be defined. Sizing is the caller's
/// ([`crate::sizing`]): this module builds what it is told to build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainSpec {
    /// libvirt domain name; see [`mint_domain_name`].
    pub name: String,
    pub vcpus: u32,
    pub memory_mib: u64,
    /// The qcow2 this boots — an overlay on the pinned guest image
    /// (`crate::guest`), never the pinned file itself, which is a base
    /// shared by every machine.
    pub disk: String,
    /// The Ignition config, as a file the guest reads once through
    /// `fw_cfg`.
    pub ignition: String,
    /// The loopback port on the host that passt forwards to the guest's
    /// sshd. The one inbound door.
    pub ssh_port: u16,
    /// The workspace this VM serves, recorded in the domain's metadata.
    pub workspace_root: String,
    /// Where the guest's serial console is written. A guest that fails to
    /// come up is read here rather than guessed at.
    pub serial_log: String,
}

/// The libvirt domain XML for a spec.
///
/// Three things in here are load-bearing and easy to lose:
///
/// - **`<sysinfo type='fwcfg'>`** is how Fedora CoreOS is configured. The
///   guest reads `opt/com.coreos/config` off the firmware config device on
///   first boot; there is no cloud-init and no image customisation step.
/// - **`<interface type='user'>` with the passt backend** puts the network
///   stack in a host process rather than in the guest.
/// - **`<portForward>`** is the only way in: host loopback to the guest's
///   port 22. It is inbound forwarding and says nothing about egress —
///   see [`GuestSpec::deny_private_networks`] for where that lives.
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
    if spec.ssh_port < 1024 {
        bail!(
            "the ssh forward must be an unprivileged port, not {}",
            spec.ssh_port
        );
    }
    let mut xml = String::new();
    let name = escape(&spec.name);
    let disk = escape(&spec.disk);
    let ignition = escape(&spec.ignition);
    let workspace = escape(&spec.workspace_root);
    let serial_log = escape(&spec.serial_log);
    writeln!(xml, "<domain type='kvm'>")?;
    writeln!(xml, "  <name>{name}</name>")?;
    writeln!(xml, "  <title>taste-ide: {workspace}</title>")?;
    // Whose VM this is, in the domain itself, so a VM libvirt has and the
    // IDE has forgotten is still attributable to a workspace.
    writeln!(xml, "  <metadata>")?;
    writeln!(xml, "    <taste:vm xmlns:taste='{METADATA_NS}'>")?;
    writeln!(xml, "      <taste:workspace>{workspace}</taste:workspace>")?;
    writeln!(xml, "    </taste:vm>")?;
    writeln!(xml, "  </metadata>")?;
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
    writeln!(
        xml,
        "      <driver name='qemu' type='qcow2' discard='unmap'/>"
    )?;
    writeln!(xml, "      <source file='{disk}'/>")?;
    writeln!(xml, "      <target dev='vda' bus='virtio'/>")?;
    writeln!(xml, "    </disk>")?;
    writeln!(xml, "    <interface type='user'>")?;
    writeln!(xml, "      <backend type='passt'/>")?;
    writeln!(xml, "      <model type='virtio'/>")?;
    // The one inbound door: host loopback → guest sshd. Loopback only, so
    // nothing else on the user's network can reach the VM.
    writeln!(xml, "      <portForward proto='tcp' address='127.0.0.1'>")?;
    writeln!(xml, "        <range start='{}' to='22'/>", spec.ssh_port)?;
    writeln!(xml, "      </portForward>")?;
    writeln!(xml, "    </interface>")?;
    // The serial console to a file, so a guest that fails to come up can be
    // read after the fact. Fedora CoreOS puts its console on ttyS0.
    writeln!(xml, "    <serial type='file'>")?;
    writeln!(xml, "      <source path='{serial_log}' append='off'/>")?;
    writeln!(xml, "      <target port='0'/>")?;
    writeln!(xml, "    </serial>")?;
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
    /// The guest's hostname, which is the domain name.
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
    /// The guest's ssh host key, when the IDE minted one ([`Keys`]). With
    /// it, the guest's identity is known before it boots; without it, sshd
    /// mints its own on first boot and the first connection has to trust
    /// whatever it finds.
    pub host_key: Option<HostKey>,
}

/// The Ignition config that makes a Fedora CoreOS guest into something the
/// IDE can use.
///
/// Deliberately small. Podman is already in FCOS, and the guest updates
/// itself, so what is said here is who may log in, what the machine is
/// called, that `core` runs podman rootless and reachable, and what the
/// guest may not talk to. Every additional thing here is a thing that has
/// to keep working across a guest release.
pub fn ignition(spec: &GuestSpec) -> Result<String> {
    if spec.ssh_public_key.trim().is_empty() {
        bail!("a guest with no ssh key is a guest the IDE cannot reach");
    }
    let core_user = serde_json::json!({ "name": GUEST_USER });
    let core_group = serde_json::json!({ "name": GUEST_USER });
    let mut files = vec![
        serde_json::json!({
            "path": "/etc/hostname",
            "mode": 420,
            "overwrite": true,
            "contents": { "source": data_url(&spec.hostname) },
        }),
        // `core` lingers, so its user manager — and the podman socket under
        // it — is up from boot rather than from a login nobody performs.
        // This is what `loginctl enable-linger core` writes.
        serde_json::json!({
            "path": format!("/var/lib/systemd/linger/{GUEST_USER}"),
            "mode": 420,
            "overwrite": true,
            "contents": { "source": "data:," },
        }),
    ];
    // The user podman.socket, enabled the way `systemctl --user enable`
    // would: a symlink into sockets.target.wants. The directories are made
    // explicitly and owned by core, because Ignition would otherwise create
    // them as root inside core's own home.
    let user_units = format!("/var/home/{GUEST_USER}/.config/systemd/user");
    let directories: Vec<serde_json::Value> = [
        format!("/var/home/{GUEST_USER}/.config"),
        format!("/var/home/{GUEST_USER}/.config/systemd"),
        user_units.clone(),
        format!("{user_units}/sockets.target.wants"),
    ]
    .into_iter()
    .map(|path| {
        serde_json::json!({
            "path": path,
            "mode": 493,
            "user": core_user,
            "group": core_group,
        })
    })
    .collect();
    let links = vec![serde_json::json!({
        "path": format!("{user_units}/sockets.target.wants/podman.socket"),
        "target": "/usr/lib/systemd/user/podman.socket",
        "user": core_user,
        "group": core_group,
    })];
    // No self-updates: an update reboots the guest, and a reboot kills
    // every container in it. Fresh releases arrive as fresh VMs.
    files.push(serde_json::json!({
        "path": "/etc/zincati/config.d/90-disable-auto-updates.toml",
        "mode": 420,
        "overwrite": true,
        "contents": { "source": data_url("[updates]\nenabled = false\n") },
    }));
    let mut units: Vec<serde_json::Value> = Vec::new();
    if let Some(host_key) = &spec.host_key {
        files.push(serde_json::json!({
            "path": "/etc/ssh/ssh_host_ed25519_key",
            "mode": 384,
            "overwrite": true,
            "contents": { "source": data_url(&host_key.private) },
        }));
        files.push(serde_json::json!({
            "path": "/etc/ssh/ssh_host_ed25519_key.pub",
            "mode": 420,
            "overwrite": true,
            "contents": { "source": data_url(&host_key.public) },
        }));
    }
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
                "name": GUEST_USER,
                "sshAuthorizedKeys": [spec.ssh_public_key.trim()],
            }],
        },
        "storage": {
            "directories": directories,
            "files": files,
            "links": links,
        },
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
    let trailing = if text.ends_with('\n') { "" } else { "%0A" };
    format!("data:,{encoded}{trailing}")
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

/// The inverse, for what comes back out of `virsh dumpxml`.
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&apos;", "'")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

// --- reading a domain back -------------------------------------------------

/// The ssh forward's host port, read off a domain's XML. The XML is the
/// record; there is no second copy of the port anywhere.
pub fn ssh_port_from_xml(xml: &str) -> Result<u16> {
    let forward = xml
        .find("<portForward")
        .context("the domain has no <portForward>; it is not one the IDE made")?;
    let range = xml[forward..]
        .find("<range")
        .map(|i| forward + i)
        .context("the <portForward> has no <range>")?;
    let start = attr(&xml[range..], "start").context("the <range> has no start")?;
    start
        .parse()
        .with_context(|| format!("the ssh forward's port is not a number: {start:?}"))
}

/// The workspace a domain serves, from its metadata. `None` for a domain
/// without one — which is a domain the IDE did not make.
pub fn workspace_from_xml(xml: &str) -> Option<PathBuf> {
    // Tolerant of the prefix libvirt hands back: it may keep `taste:` or
    // rewrite it, and the element's local name is what matters.
    let open = xml.find("workspace>")?;
    let text_start = open + "workspace>".len();
    let text_end = text_start + xml[text_start..].find("</")?;
    let text = unescape(xml[text_start..text_end].trim());
    (!text.is_empty()).then(|| PathBuf::from(text))
}

/// `name='value'` or `name="value"` in an XML fragment.
fn attr<'a>(fragment: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("{name}=");
    let at = fragment.find(&key)? + key.len();
    let quote = fragment[at..].chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let value_start = at + 1;
    let value_end = value_start + fragment[value_start..].find(quote)?;
    Some(&fragment[value_start..value_end])
}

/// What `virsh domstate` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainState {
    Running,
    ShutOff,
    /// paused, crashed, in shutdown, pmsuspended — states the IDE does not
    /// bring a VM up from by itself.
    Other(String),
}

impl DomainState {
    pub fn parse(text: &str) -> Self {
        match text.trim() {
            "running" => Self::Running,
            "shut off" => Self::ShutOff,
            other => Self::Other(other.to_string()),
        }
    }
}

/// One VM the IDE made, as libvirt has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vm {
    /// The libvirt domain — and the podman connection, one name for both.
    pub domain: String,
    pub ssh_port: u16,
    pub workspace_root: PathBuf,
    pub state: DomainState,
}

impl Vm {
    /// The podman connection this VM is reached through. It is the domain
    /// name: the substrate wants one word for a VM, and there is no reason
    /// to have two.
    pub fn connection(&self) -> &str {
        &self.domain
    }

    /// The URI `podman system connection add` is given.
    pub fn podman_uri(&self) -> String {
        format!(
            "ssh://{GUEST_USER}@127.0.0.1:{}{GUEST_PODMAN_SOCKET}",
            self.ssh_port
        )
    }

    pub fn disk_path(&self) -> PathBuf {
        disks_dir().join(format!("{}.qcow2", self.domain))
    }

    fn guest_dir(&self) -> PathBuf {
        Keys::for_workspace(&self.workspace_root)
            .dir()
            .to_path_buf()
    }

    pub fn ignition_path(&self) -> PathBuf {
        self.guest_dir().join(format!("{}.ign", self.domain))
    }

    pub fn serial_log_path(&self) -> PathBuf {
        self.guest_dir().join(format!("{}.serial.log", self.domain))
    }
}

/// What a VM costs, as configured and as measured.
///
/// Two of these are configuration read back from libvirt and one is the
/// disk's allocation on the host, and they are kept apart on purpose: the
/// memory number is what the host *loses* (no balloon, so RSS climbs to it
/// and stays), while the disk number is what the VM has *taken so far* of
/// a sparse ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmFacts {
    pub running: bool,
    pub cpus: u64,
    /// Configured memory. Per the spike this is also the host RSS ceiling,
    /// which is why it is reported as a commitment.
    pub memory_mib: u64,
    pub disk_ceiling_gib: u64,
    /// Bytes the VM's disk actually occupies on the host. `None` when it
    /// could not be measured, never zero: a footprint that silently
    /// under-reports is worse than one that says it could not see.
    pub host_storage_bytes: Option<u64>,
}

impl VmFacts {
    /// One line for the Resources view.
    pub fn summary(&self) -> String {
        let mut parts = vec![
            if self.running { "running" } else { "stopped" }.to_string(),
            format!("{} vCPU", self.cpus),
            format!("{} committed", gib(self.memory_mib * 1024 * 1024)),
        ];
        match self.host_storage_bytes {
            Some(bytes) => parts.push(format!(
                "{} on disk of {} GiB",
                gib(bytes),
                self.disk_ceiling_gib
            )),
            None => parts.push(format!(
                "disk unmeasured, {} GiB ceiling",
                self.disk_ceiling_gib
            )),
        }
        parts.join(", ")
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// `virsh dominfo`, parsed. The fields are libvirt's documented output,
/// and a field that is missing falls back to the sizing that would have
/// been asked for, so a VM that cannot be described is still a row and not
/// a panic.
pub fn facts_from_dominfo(
    info: &str,
    disk_ceiling_gib: u64,
    host_storage_bytes: Option<u64>,
) -> VmFacts {
    let field = |name: &str| -> Option<String> {
        info.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == name).then(|| value.trim().to_string())
        })
    };
    let number = |name: &str| -> Option<u64> {
        field(name).and_then(|v| v.split_whitespace().next()?.parse().ok())
    };
    let sizing = Sizing::for_host();
    VmFacts {
        running: field("State").is_some_and(|s| s == "running"),
        cpus: number("CPU(s)").unwrap_or(u64::from(sizing.vcpus)),
        memory_mib: number("Max memory")
            .map(|kib| kib / 1024)
            .unwrap_or(sizing.memory_mib),
        disk_ceiling_gib,
        host_storage_bytes,
    }
}

// --- the provisioner --------------------------------------------------------

/// User-session libvirt on this host. The default provisioner, and the one
/// with no credential: it is the user.
#[derive(Debug, Clone)]
pub struct LibvirtSession {
    sandboxed: bool,
}

impl Default for LibvirtSession {
    fn default() -> Self {
        Self::new()
    }
}

impl LibvirtSession {
    pub fn new() -> Self {
        Self {
            sandboxed: taste_core::podman::sandboxed(),
        }
    }

    /// One phrase, for notes and the log.
    pub fn describe(&self) -> &'static str {
        "local libvirt (qemu:///session)"
    }

    async fn run(&self, program: &str, args: Vec<String>) -> Result<String> {
        let (program, args) = host_argv(self.sandboxed, program, args);
        let output = tokio::process::Command::new(&program)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .with_context(|| format!("running {program}"))?;
        if !output.status.success() {
            bail!(
                "{program} {}: {}",
                args.first().map(String::as_str).unwrap_or_default(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    async fn virsh(&self, args: &[&str]) -> Result<String> {
        let mut argv = vec!["-c".to_string(), SESSION_URI.to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
        self.run("virsh", argv).await
    }

    /// Can this host provision at all? Each refusal names what is missing,
    /// because on a stock Silverblue or standard Bluefin the answer is no
    /// and the user's way forward is to layer libvirt and qemu or to point
    /// a cloud provisioner at an account (docs/ENVIRONMENTS.md → "Host
    /// packaging, settled").
    pub async fn available(&self) -> Result<()> {
        self.virsh(&["uri"])
            .await
            .context("libvirt's session daemon did not answer; is libvirt installed?")?;
        self.run("qemu-img", vec!["--version".into()])
            .await
            .context("qemu-img is not installed; the VM's disk needs it")?;
        self.run("passt", vec!["--version".into()])
            .await
            .context("passt is not installed; the VM's network backend needs it")?;
        Ok(())
    }

    /// Every VM whose name starts with `prefix`, sorted by name.
    async fn list_prefixed(&self, prefix: &str) -> Result<Vec<Vm>> {
        let names = self.virsh(&["list", "--all", "--name"]).await?;
        let mut vms = Vec::new();
        for name in names
            .lines()
            .map(str::trim)
            .filter(|n| n.starts_with(prefix))
        {
            let xml = self.virsh(&["dumpxml", name]).await?;
            let state = self.virsh(&["domstate", name]).await?;
            vms.push(Vm {
                domain: name.to_string(),
                ssh_port: ssh_port_from_xml(&xml)
                    .with_context(|| format!("reading {name}'s ssh forward"))?,
                workspace_root: workspace_from_xml(&xml).unwrap_or_default(),
                state: DomainState::parse(&state),
            });
        }
        vms.sort_by(|a, b| a.domain.cmp(&b.domain));
        Ok(vms)
    }

    /// This workspace's pool, as libvirt has it.
    pub async fn list(&self, workspace_root: &Path) -> Result<Vec<Vm>> {
        self.list_prefixed(&domain_prefix(workspace_root)).await
    }

    /// Every VM the IDE made, for any workspace — the startup sweep's view.
    pub async fn list_all(&self) -> Result<Vec<Vm>> {
        self.list_prefixed("taste-").await
    }

    /// Make a VM for a workspace: the disk, the Ignition, the definition.
    /// Defined and shut off; [`Self::start`] and [`Self::wait_ready`] bring
    /// it up.
    ///
    /// `progress` is the base image's download, when this host has never
    /// had it — about a gigabyte, once.
    pub async fn create(
        &self,
        workspace_root: &Path,
        sizing: &Sizing,
        progress: impl Fn(u64, u64) + Send + Sync + 'static,
    ) -> Result<Vm> {
        crate::sizing::check_free_space(&disks_dir())?;
        let image = crate::guest::image()?;
        let base = image.ensure_base(self.sandboxed, progress).await?;

        let keys = Keys::for_workspace(workspace_root);
        keys.ensure().await?;

        let domain = mint_domain_name(workspace_root)?;
        let ssh_port = free_loopback_port()?;
        let vm = Vm {
            domain: domain.clone(),
            ssh_port,
            workspace_root: workspace_root.to_path_buf(),
            state: DomainState::ShutOff,
        };

        let ignition_text = ignition(&GuestSpec {
            ssh_public_key: keys.identity_public()?,
            hostname: domain.clone(),
            deny_private_networks: true,
            host_key: Some(keys.host_key()?),
        })?;
        write_private(&vm.ignition_path(), &ignition_text)?;

        let disk = vm.disk_path();
        std::fs::create_dir_all(disks_dir())
            .with_context(|| format!("creating {}", disks_dir().display()))?;
        self.run(
            "qemu-img",
            vec![
                "create".into(),
                "-q".into(),
                "-f".into(),
                "qcow2".into(),
                "-b".into(),
                base.display().to_string(),
                "-F".into(),
                "qcow2".into(),
                disk.display().to_string(),
                format!("{}G", sizing.disk_gib),
            ],
        )
        .await
        .context("creating the VM's disk")?;

        let xml = domain_xml(&DomainSpec {
            name: domain.clone(),
            vcpus: sizing.vcpus,
            memory_mib: sizing.memory_mib,
            disk: disk.display().to_string(),
            ignition: vm.ignition_path().display().to_string(),
            ssh_port,
            workspace_root: workspace_root.display().to_string(),
            serial_log: vm.serial_log_path().display().to_string(),
        })?;
        let xml_path = keys.dir().join(format!("{domain}.xml"));
        std::fs::write(&xml_path, xml)
            .with_context(|| format!("writing {}", xml_path.display()))?;
        let defined = self
            .virsh(&["define", &xml_path.display().to_string()])
            .await
            .context("defining the domain");
        let _ = std::fs::remove_file(&xml_path);
        if let Err(e) = defined {
            let _ = std::fs::remove_file(&disk);
            let _ = std::fs::remove_file(vm.ignition_path());
            return Err(e);
        }
        keys.record_host(ssh_port)?;
        tracing::info!(
            "defined VM {domain} for {} ({} vCPU, {} MiB, ssh on 127.0.0.1:{ssh_port})",
            workspace_root.display(),
            sizing.vcpus,
            sizing.memory_mib
        );
        Ok(vm)
    }

    pub async fn state(&self, vm: &Vm) -> Result<DomainState> {
        Ok(DomainState::parse(
            &self.virsh(&["domstate", &vm.domain]).await?,
        ))
    }

    pub async fn start(&self, vm: &Vm) -> Result<()> {
        self.virsh(&["start", &vm.domain])
            .await
            .with_context(|| format!("starting {}", vm.domain))?;
        Ok(())
    }

    /// Wait for the guest to be usable: sshd answering on the forward, the
    /// podman connection registered, and podman in the guest answering
    /// through it. A deadline missed comes back with the serial console's
    /// last lines, which is where a guest that did not boot says why.
    pub async fn wait_ready(&self, vm: &Vm, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if tokio::net::TcpStream::connect(("127.0.0.1", vm.ssh_port))
                .await
                .is_ok()
            {
                break;
            }
            if Instant::now() > deadline {
                bail!(
                    "{} did not open ssh on 127.0.0.1:{} within {}s\n{}",
                    vm.domain,
                    vm.ssh_port,
                    timeout.as_secs(),
                    serial_tail(&vm.serial_log_path(), 20)
                );
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        self.register_connection(vm).await?;
        let target = PodmanTarget::connection(&vm.domain, self.sandboxed);
        loop {
            match crate::substrate::probe(&target).await {
                Ok(()) => return Ok(()),
                Err(e) if Instant::now() > deadline => bail!(
                    "podman in {} did not answer over connection {} within {}s ({e})\n{}",
                    vm.domain,
                    vm.domain,
                    timeout.as_secs(),
                    serial_tail(&vm.serial_log_path(), 20)
                ),
                Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
    }

    /// `podman system connection add`, named for the domain, with the
    /// workspace's identity. Replaces a stale one of the same name.
    async fn register_connection(&self, vm: &Vm) -> Result<()> {
        let keys = Keys::for_workspace(&vm.workspace_root);
        let podman = PodmanTarget::local(self.sandboxed);
        let (program, args) = podman.argv(["system", "connection", "remove", &vm.domain]);
        let _ = tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .await;
        let (program, args) = podman.argv([
            "system".to_string(),
            "connection".into(),
            "add".into(),
            "--identity".into(),
            keys.identity().display().to_string(),
            vm.domain.clone(),
            vm.podman_uri(),
        ]);
        let output = tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .context("running podman system connection add")?;
        if !output.status.success() {
            bail!(
                "registering the podman connection {}: {}",
                vm.domain,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    /// Bring a VM up if it is not, and report what it costs.
    pub async fn ensure_running(&self, vm: &Vm) -> Result<VmFacts> {
        match self.state(vm).await? {
            DomainState::Running => {}
            DomainState::ShutOff => self.start(vm).await?,
            DomainState::Other(state) => bail!(
                "{} is {state}, which is not a state the IDE brings a VM up from",
                vm.domain
            ),
        }
        self.wait_ready(vm, READY_TIMEOUT).await?;
        self.facts(vm).await
    }

    /// ACPI shutdown, **spawned and not waited for** — for the window's
    /// close handler, which runs on the GTK thread and is followed at once
    /// by the process's exit. `virsh shutdown` returns as soon as the
    /// signal is sent; a detached child outlives this process, so the
    /// guest gets its signal whether or not the IDE is still there to hear
    /// the answer.
    pub fn shutdown_detached(&self, domain: &str) -> std::io::Result<()> {
        let (program, args) = host_argv(
            self.sandboxed,
            "virsh",
            ["-c", SESSION_URI, "shutdown", domain],
        );
        std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|_| ())
    }

    /// ACPI shutdown. The guest stops cleanly and keeps its disk; the next
    /// `start` is a warm boot.
    pub async fn stop(&self, vm: &Vm) -> Result<()> {
        match self.virsh(&["shutdown", &vm.domain]).await {
            Ok(_) => Ok(()),
            Err(e) if format!("{e:#}").contains("not running") => Ok(()),
            Err(e) => Err(e).with_context(|| format!("shutting down {}", vm.domain)),
        }
    }

    /// Remove a VM entirely: the domain, its disk, its Ignition, its log,
    /// its connection, and its `known_hosts` line. The base image stays.
    pub async fn destroy(&self, vm: &Vm) -> Result<()> {
        if self.state(vm).await? == DomainState::Running {
            let _ = self.virsh(&["destroy", &vm.domain]).await;
        }
        self.virsh(&["undefine", &vm.domain])
            .await
            .with_context(|| format!("undefining {}", vm.domain))?;
        for path in [vm.disk_path(), vm.ignition_path(), vm.serial_log_path()] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!("removing {}: {e}", path.display()),
            }
        }
        let podman = PodmanTarget::local(self.sandboxed);
        let (program, args) = podman.argv(["system", "connection", "remove", &vm.domain]);
        let _ = tokio::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .await;
        Keys::for_workspace(&vm.workspace_root).forget_host(vm.ssh_port)?;
        tracing::info!("destroyed VM {}", vm.domain);
        Ok(())
    }

    /// What the VM is configured as and what it has taken.
    pub async fn facts(&self, vm: &Vm) -> Result<VmFacts> {
        let info = self.virsh(&["dominfo", &vm.domain]).await?;
        let disk = vm.disk_path();
        let ceiling = match self
            .run(
                "qemu-img",
                vec![
                    "info".into(),
                    "--output=json".into(),
                    disk.display().to_string(),
                ],
            )
            .await
        {
            Ok(json) => serde_json::from_str::<serde_json::Value>(&json)
                .ok()
                .and_then(|v| v.get("virtual-size")?.as_u64())
                .map(|bytes| bytes / (1024 * 1024 * 1024))
                .unwrap_or(crate::sizing::DISK_GIB),
            Err(_) => crate::sizing::DISK_GIB,
        };
        Ok(facts_from_dominfo(&info, ceiling, allocated_bytes(&disk)))
    }
}

/// Bytes a file actually occupies — the sparse answer, which for a qcow2
/// overlay is the only honest one.
fn allocated_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.blocks() * 512)
}

/// A loopback port nothing holds right now, chosen by the kernel and
/// released at once. passt binds it when the VM starts; the gap is small
/// and a collision fails loudly at `virsh start`.
fn free_loopback_port() -> Result<u16> {
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).context("finding a free loopback port")?;
    Ok(listener.local_addr()?.port())
}

/// The last `lines` of the serial console, or a note that there is none.
fn serial_tail(path: &Path, lines: usize) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let all: Vec<&str> = text.lines().collect();
            let start = all.len().saturating_sub(lines);
            format!(
                "serial console ({}):\n{}",
                path.display(),
                all[start..].join("\n")
            )
        }
        Err(_) => format!("no serial console at {}", path.display()),
    }
}

/// Written `0600` from the first byte: the Ignition carries the guest's
/// private host key.
fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> DomainSpec {
        DomainSpec {
            name: "taste-799fd7acd369bf5c-k7m2qx".into(),
            vcpus: 8,
            memory_mib: 10240,
            disk: "/home/dev/.local/share/taste-ide/guests/machines/ws.qcow2".into(),
            ignition: "/home/dev/.local/state/taste-ide/workspaces/ws/guest/ws.ign".into(),
            ssh_port: 40022,
            workspace_root: "/home/dev/Projects/ws".into(),
            serial_log: "/home/dev/.local/state/taste-ide/workspaces/ws/guest/ws.serial.log".into(),
        }
    }

    fn guest() -> GuestSpec {
        GuestSpec {
            ssh_public_key: "ssh-ed25519 AAAAC3Nz taste@host".into(),
            hostname: "taste-sb".into(),
            deny_private_networks: false,
            host_key: None,
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
    /// entry, the CPU mode, the port forward, and the metadata through its
    /// own normalisation, which is what their being load-bearing means in
    /// practice.
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
        assert!(xml.contains("<name>taste-799fd7acd369bf5c-k7m2qx</name>"));
        assert!(xml.contains("<memory unit='MiB'>10240</memory>"));
        assert!(xml.contains("<vcpu placement='static'>8</vcpu>"));
    }

    /// The one inbound door, on loopback, to sshd — and nothing else is
    /// forwarded.
    #[test]
    fn the_only_forward_is_inbound_ssh_on_loopback() {
        let xml = domain_xml(&spec()).unwrap();
        assert_eq!(xml.matches("<portForward").count(), 1, "{xml}");
        assert!(
            xml.contains("<portForward proto='tcp' address='127.0.0.1'>"),
            "loopback only:\n{xml}"
        );
        assert!(xml.contains("<range start='40022' to='22'/>"), "{xml}");
        assert_eq!(ssh_port_from_xml(&xml).unwrap(), 40022);
        // A privileged port would need root passt does not have.
        let mut bad = spec();
        bad.ssh_port = 22;
        assert!(domain_xml(&bad).is_err());
    }

    /// The workspace rides in the domain and comes back out of it, with
    /// the escaping a real path may need.
    #[test]
    fn the_domain_names_its_workspace_and_gives_it_back() {
        let mut awkward = spec();
        awkward.workspace_root = "/home/dev/a & b/<proj>".into();
        let xml = domain_xml(&awkward).unwrap();
        assert!(
            xml.contains(&format!("xmlns:taste='{METADATA_NS}'")),
            "{xml}"
        );
        assert_eq!(
            workspace_from_xml(&xml),
            Some(PathBuf::from("/home/dev/a & b/<proj>"))
        );
        assert!(xml.contains("<title>taste-ide: /home/dev/a &amp; b/&lt;proj&gt;</title>"));
        // A domain the IDE did not make has neither.
        assert_eq!(workspace_from_xml("<domain><name>x</name></domain>"), None);
        assert!(ssh_port_from_xml("<domain><name>x</name></domain>").is_err());
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

    /// The guest says who may log in, what it is called, and that core
    /// runs podman rootless and reachable — and nothing else that has to
    /// survive a release.
    #[test]
    fn the_ignition_config_is_a_key_a_name_and_a_rootless_podman() {
        let config = ignition(&guest()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&config).unwrap();
        assert_eq!(parsed["ignition"]["version"], "3.4.0");
        assert_eq!(parsed["passwd"]["users"][0]["name"], "core");
        assert_eq!(
            parsed["passwd"]["users"][0]["sshAuthorizedKeys"][0],
            "ssh-ed25519 AAAAC3Nz taste@host"
        );
        let files = parsed["storage"]["files"].as_array().unwrap();
        let hostname = files
            .iter()
            .find(|f| f["path"] == "/etc/hostname")
            .expect("a hostname")["contents"]["source"]
            .as_str()
            .unwrap();
        assert!(hostname.starts_with("data:,taste-sb"), "{hostname}");
        // Rootless as core: linger, and the USER socket, not root's.
        assert!(
            files
                .iter()
                .any(|f| f["path"] == "/var/lib/systemd/linger/core"),
            "core must linger for its podman socket to be up from boot"
        );
        let links = parsed["storage"]["links"].as_array().unwrap();
        let socket = links
            .iter()
            .find(|l| l["target"] == "/usr/lib/systemd/user/podman.socket")
            .expect("the user podman.socket is enabled");
        assert!(
            socket["path"]
                .as_str()
                .unwrap()
                .starts_with("/var/home/core/.config/systemd/user/sockets.target.wants/"),
            "{socket}"
        );
        assert_eq!(socket["user"]["name"], "core");
        let system_units: Vec<&str> = parsed["systemd"]["units"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|u| u["name"].as_str())
            .collect();
        assert!(
            !system_units.contains(&"podman.socket"),
            "root's podman socket is not the one the IDE talks to: {system_units:?}"
        );
        // The directories in core's home are core's, not root's.
        for dir in parsed["storage"]["directories"].as_array().unwrap() {
            assert_eq!(dir["user"]["name"], "core", "{dir}");
        }
    }

    /// The guest's identity is decided before it boots.
    #[test]
    fn a_minted_host_key_is_installed_for_sshd() {
        let mut spec = guest();
        spec.host_key = Some(HostKey {
            private:
                "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n"
                    .into(),
            public: "ssh-ed25519 AAAAhost taste-ide guest host key".into(),
        });
        let parsed: serde_json::Value = serde_json::from_str(&ignition(&spec).unwrap()).unwrap();
        let files = parsed["storage"]["files"].as_array().unwrap();
        let private = files
            .iter()
            .find(|f| f["path"] == "/etc/ssh/ssh_host_ed25519_key")
            .expect("the private host key");
        assert_eq!(private["mode"], 0o600);
        assert!(
            private["contents"]["source"]
                .as_str()
                .unwrap()
                .contains("BEGIN%20OPENSSH%20PRIVATE%20KEY"),
            "{private}"
        );
        let public = files
            .iter()
            .find(|f| f["path"] == "/etc/ssh/ssh_host_ed25519_key.pub")
            .expect("the public host key");
        assert_eq!(public["mode"], 0o644);
        // Without one, nothing is installed and sshd mints its own.
        assert!(!ignition(&guest()).unwrap().contains("ssh_host_ed25519_key"));
    }

    /// The denial the standard needs, in the one place it can be made.
    #[test]
    fn a_guest_can_be_kept_off_the_users_network() {
        let mut spec = guest();
        spec.deny_private_networks = true;
        let parsed: serde_json::Value = serde_json::from_str(&ignition(&spec).unwrap()).unwrap();
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

    /// The guest never updates itself: an update is a reboot, and a reboot
    /// is every container in it killed. Fresh releases are fresh VMs.
    #[test]
    fn the_guest_has_auto_updates_switched_off() {
        let parsed: serde_json::Value = serde_json::from_str(&ignition(&guest()).unwrap()).unwrap();
        let zincati = parsed["storage"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == "/etc/zincati/config.d/90-disable-auto-updates.toml")
            .expect("zincati is configured");
        let source = zincati["contents"]["source"].as_str().unwrap();
        assert!(source.contains("enabled%20%3D%20false"), "{source}");
    }

    /// A guest not asked to be isolated carries no ruleset at all, rather
    /// than an empty one that reads like a policy.
    #[test]
    fn a_guest_not_asked_to_be_isolated_carries_no_ruleset() {
        assert!(!ignition(&guest()).unwrap().contains("nftables"));
    }

    #[test]
    fn a_guest_with_no_key_is_refused() {
        let mut spec = guest();
        spec.ssh_public_key = "  ".into();
        assert!(
            ignition(&spec).is_err(),
            "the IDE could never reach that VM"
        );
    }

    /// A data URL ends the text with one newline, whether or not the text
    /// brought its own — a key file with two trailing newlines is still a
    /// key file, but a hostname with none is not a hostname.
    #[test]
    fn a_data_url_ends_with_exactly_one_newline() {
        assert!(data_url("host").ends_with("host%0A"));
        assert!(data_url("key\n").ends_with("key%0A"));
        assert!(!data_url("key\n").ends_with("%0A%0A"));
    }

    /// Names are the prefix and six characters of entropy — never a
    /// counter — and two draws differ.
    #[test]
    fn domain_names_carry_the_workspace_and_entropy_not_a_counter() {
        let root = Path::new("/work/proj");
        let prefix = domain_prefix(root);
        assert!(prefix.starts_with("taste-"));
        assert!(prefix.ends_with('-'));
        let a = mint_domain_name(root).unwrap();
        let b = mint_domain_name(root).unwrap();
        assert!(a.starts_with(&prefix) && b.starts_with(&prefix));
        assert_eq!(a.len(), prefix.len() + NAME_ENTROPY);
        assert!(a[prefix.len()..]
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        assert_ne!(a, b);
        // A domain name is also a valid hostname and a valid podman
        // connection name: no whitespace, no dots to confuse a resolver.
        assert!(!a.contains(|c: char| c.is_whitespace() || c == '.'));
    }

    /// `virsh dominfo` as libvirt prints it, into the row's numbers.
    #[test]
    fn dominfo_is_read_as_libvirt_prints_it() {
        let info = "Id:             3\n\
                    Name:           taste-799f-k7m2qx\n\
                    UUID:           8f4e6e18-ab9e-42a4-bbf7-112d2d83f4a4\n\
                    OS Type:        hvm\n\
                    State:          running\n\
                    CPU(s):         12\n\
                    CPU time:       55.3s\n\
                    Max memory:     10485760 KiB\n\
                    Used memory:    10485760 KiB\n\
                    Persistent:     yes\n\
                    Autostart:      disable\n";
        let facts = facts_from_dominfo(info, 64, Some(3 * 1024 * 1024 * 1024));
        assert!(facts.running);
        assert_eq!(facts.cpus, 12);
        assert_eq!(facts.memory_mib, 10240);
        let summary = facts.summary();
        assert!(
            summary.contains("running") && summary.contains("12 vCPU"),
            "{summary}"
        );
        assert!(summary.contains("10.0 GiB committed"), "{summary}");
        assert!(summary.contains("3.0 GiB on disk of 64 GiB"), "{summary}");
        let stopped = facts_from_dominfo(&info.replace("running", "shut off"), 64, None);
        assert!(!stopped.running);
        assert!(stopped.summary().contains("unmeasured"));
    }

    #[test]
    fn domain_states_are_the_two_the_ide_acts_on_and_the_rest() {
        assert_eq!(DomainState::parse("running\n"), DomainState::Running);
        assert_eq!(DomainState::parse("shut off"), DomainState::ShutOff);
        assert_eq!(
            DomainState::parse("paused"),
            DomainState::Other("paused".into())
        );
    }

    /// The connection is the domain, and the URI names the rootless
    /// socket of the guest's user through the ssh forward.
    #[test]
    fn a_vm_is_reached_as_core_through_its_forward() {
        let vm = Vm {
            domain: "taste-799f-k7m2qx".into(),
            ssh_port: 40022,
            workspace_root: "/work/proj".into(),
            state: DomainState::ShutOff,
        };
        assert_eq!(vm.connection(), "taste-799f-k7m2qx");
        assert_eq!(
            vm.podman_uri(),
            "ssh://core@127.0.0.1:40022/run/user/1000/podman/podman.sock"
        );
        assert!(vm
            .disk_path()
            .ends_with("guests/machines/taste-799f-k7m2qx.qcow2"));
        assert!(vm.ignition_path().ends_with("guest/taste-799f-k7m2qx.ign"));
    }
}
