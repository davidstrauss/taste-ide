//! Wake the machine behind a private model when it has gone to sleep.
//!
//! A private server lives on a machine of the user's, and that machine
//! sleeps. The idle timeout ([`crate::proxy`]) ends a stream the machine
//! stopped feeding; this is the other half — the next turn, or the
//! settings form's connection test, finds the server unreachable and,
//! rather than failing, sends a Wake-on-LAN packet and waits for it to
//! come up (David, 2026-09-16: "Add this support. Mention in chat/
//! connection check that it's attempting it (if the API seems
//! unavailable), but don't make it configurable. Just do it and provide
//! transparency").
//!
//! # Nothing to configure
//!
//! Wake-on-LAN needs the machine's hardware address, and nobody is asked
//! for it: the first time the server answers, the IDE reads the address
//! the kernel already has for it — the neighbour table, `/proc/net/arp`,
//! which records the MAC of every LAN host this machine has spoken to —
//! and keeps it beside the private-model file
//! (`private-model-wake.json`, IDE state, never the checkout). It is
//! re-learned once a day, so a machine whose address changed is caught
//! up with. The table is read over netlink (`RTM_GETNEIGH`), which is
//! the one way to read the IPv6 half: ARP is IPv4's protocol and
//! `/proc/net/arp` its table, while IPv6 resolves link-layer addresses
//! with Neighbor Discovery and keeps those entries where only netlink
//! reaches (David, 2026-09-16: "Add the necessary IPv6 support, too").
//! One dump covers both families, so whichever address the server's name
//! resolves to first is the one that gets looked up.
//!
//! # What "attempting" looks like
//!
//! Every step is said where the user is: a notice into the chat whose
//! turn hit the sleeping server (the proxy's notice hook, `Event::ChatNotice`
//! on the app's side), or the connection test's own verdict. Not
//! answering, wake-up sent to which address, answered after how long, or
//! still asleep after the wait — each is a sentence, because a turn that
//! silently takes forty seconds longer than usual is a turn the user
//! wonders about.
//!
//! # What it cannot do
//!
//! The machine's firmware and NIC have to allow wake from a magic
//! packet, and Windows' fast startup can leave the NIC unpowered after a
//! shutdown (a sleep is fine). The IDE sends the packet as a broadcast on
//! the LAN — the limited broadcast and the neighbour's own subnet for an
//! IPv4 neighbour, all-nodes multicast on the neighbour's interface for
//! an IPv6 one; the NIC matches the magic pattern whatever frame carries
//! it. A server on another network is out of reach of a broadcast, which
//! is the protocol's limit and not this module's.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use http::Uri;
use serde::{Deserialize, Serialize};

/// What the IDE learned about the machine behind a private server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Neighbor {
    /// The host as the private-model file names it.
    pub host: String,
    /// The address it resolved to when the MAC was read.
    pub ip: IpAddr,
    /// The hardware address, as the neighbour table gave it.
    pub mac: [u8; 6],
    /// The interface the neighbour was seen on, which an IPv6 wake-up is
    /// sent out of. Zero on a record from before it was kept.
    #[serde(default)]
    pub ifindex: u32,
    /// When it was learned, seconds since the epoch.
    pub learned: i64,
}

impl Neighbor {
    pub fn mac_text(&self) -> String {
        self.mac
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }
}

/// How one wake attempt went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wake {
    /// The server accepted a connection; nothing to wake.
    Reachable,
    /// A wake-up was sent and the server came up.
    Woke {
        neighbor: Neighbor,
        waited: Duration,
    },
    /// A wake-up was sent and the server did not come up in time.
    StillAsleep {
        neighbor: Neighbor,
        waited: Duration,
    },
    /// Unreachable, and nothing known to wake: the server has never
    /// answered from a LAN address this machine could read the MAC of.
    NoNeighbor,
    /// The packet could not be sent at all.
    Unsent { neighbor: Neighbor, error: String },
}

impl Wake {
    /// The sentence for the user, or `None` when there is nothing to say
    /// (the server was simply there).
    pub fn note(&self, host: &str) -> Option<String> {
        match self {
            Wake::Reachable => None,
            Wake::Woke { neighbor, waited } => Some(format!(
                "{host} was asleep — woke it ({}) and it answered after {}s",
                neighbor.mac_text(),
                waited.as_secs()
            )),
            Wake::StillAsleep { neighbor, waited } => Some(format!(
                "{host} is not answering; a wake-up was sent to {} and it had not come up \
                 after {}s. It may be off, or its network card may not allow waking.",
                neighbor.mac_text(),
                waited.as_secs()
            )),
            Wake::NoNeighbor => Some(format!(
                "{host} is not answering, and it cannot be woken yet: the IDE learns the \
                 machine's hardware address the first time the server answers from the LAN."
            )),
            Wake::Unsent { neighbor, error } => Some(format!(
                "{host} is not answering, and the wake-up to {} could not be sent: {error}",
                neighbor.mac_text()
            )),
        }
    }

    pub fn reached(&self) -> bool {
        matches!(self, Wake::Reachable | Wake::Woke { .. })
    }
}

/// How long the wake loop waits for the machine, by default. A PC coming
/// out of sleep is up in a few seconds; one coming out of hibernation
/// takes tens.
pub const WAKE_WAIT: Duration = Duration::from_secs(60);
/// How long one connection attempt gets before the server counts as not
/// there. On a LAN a live server accepts in milliseconds.
const CONNECT_PROBE: Duration = Duration::from_millis(1500);
/// Between connection attempts while waiting for the wake.
const RETRY_EVERY: Duration = Duration::from_secs(3);
/// How old a learned address may be before it is read again.
const RELEARN_AFTER: i64 = 24 * 60 * 60;

/// Where the learned address lives: beside the private-model file it is
/// about.
pub fn wake_path(private_model_path: &Path) -> PathBuf {
    private_model_path.with_file_name("private-model-wake.json")
}

fn host_and_port(uri: &Uri) -> Result<(String, u16)> {
    let host = uri.host().context("the private model's URL has no host")?;
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("https") {
            443
        } else {
            80
        });
    Ok((host.to_string(), port))
}

/// Does the server accept a connection right now?
pub async fn reachable(uri: &Uri) -> bool {
    let Ok((host, port)) = host_and_port(uri) else {
        return false;
    };
    let connect = tokio::net::TcpStream::connect((host.as_str(), port));
    matches!(
        tokio::time::timeout(CONNECT_PROBE, connect).await,
        Ok(Ok(_))
    )
}

/// The neighbour on file, if one was learned.
pub async fn remembered(path: &Path) -> Option<Neighbor> {
    let bytes = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Read the server machine's hardware address off the kernel's neighbour
/// table and keep it. Called after the server has answered — the one
/// moment the table is sure to have the entry — and a no-op when what is
/// on file is fresh. Best effort throughout: nothing here can fail a
/// turn.
pub async fn learn(uri: &Uri, path: &Path) -> Option<Neighbor> {
    if let Some(known) = remembered(path).await {
        if now_seconds() - known.learned < RELEARN_AFTER {
            return Some(known);
        }
    }
    let (host, port) = host_and_port(uri).ok()?;
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .ok()?
        .collect();
    // The kernel's neighbour table, both families, off the request path.
    let table = tokio::task::spawn_blocking(netlink::neighbours)
        .await
        .ok()?
        .ok()?;
    let neighbor = addrs.iter().find_map(|addr| {
        let entry = table.iter().find(|entry| entry.ip == addr.ip())?;
        Some(Neighbor {
            host: host.clone(),
            ip: entry.ip,
            mac: entry.mac,
            ifindex: entry.ifindex,
            learned: now_seconds(),
        })
    })?;
    if let Ok(bytes) = serde_json::to_vec_pretty(&neighbor) {
        if let Some(dir) = path.parent() {
            let _ = tokio::fs::create_dir_all(dir).await;
        }
        let _ = tokio::fs::write(path, bytes).await;
    }
    Some(neighbor)
}

/// The Wake-on-LAN magic packet: six `0xff`, then the MAC sixteen times.
pub fn magic_packet(mac: &[u8; 6]) -> [u8; 102] {
    let mut packet = [0xffu8; 102];
    for copy in 0..16 {
        packet[6 + copy * 6..12 + copy * 6].copy_from_slice(mac);
    }
    packet
}

/// Send the wake-up on the discard port and the two ports WoL tools use:
/// for an IPv4 neighbour the limited broadcast and its own /24 broadcast,
/// for an IPv6 one the all-nodes multicast out of the interface it was
/// seen on. The NIC matches the magic pattern in the frame's payload
/// whatever addresses carry it.
pub async fn send(neighbor: &Neighbor) -> Result<()> {
    let packet = magic_packet(&neighbor.mac);
    let mut sent = 0;
    let mut last_error = None;
    let targets: Vec<SocketAddr> = match neighbor.ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            let subnet = Ipv4Addr::new(octets[0], octets[1], octets[2], 255);
            [Ipv4Addr::BROADCAST, subnet]
                .into_iter()
                .flat_map(|target| [9u16, 7, 40000].map(|port| SocketAddr::from((target, port))))
                .collect()
        }
        IpAddr::V6(_) => {
            let all_nodes = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1);
            [9u16, 7, 40000]
                .into_iter()
                .map(|port| SocketAddr::V6(SocketAddrV6::new(all_nodes, port, 0, neighbor.ifindex)))
                .collect()
        }
    };
    let bind: SocketAddr = match neighbor.ip {
        IpAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        IpAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = tokio::net::UdpSocket::bind(bind)
        .await
        .context("opening a UDP socket for the wake-up")?;
    socket
        .set_broadcast(true)
        .context("allowing broadcast on the wake-up socket")?;
    for target in targets {
        match socket.send_to(&packet, target).await {
            Ok(_) => sent += 1,
            Err(e) => last_error = Some(e),
        }
    }
    if sent == 0 {
        return Err(anyhow::anyhow!(
            "{}",
            last_error
                .map(|e| e.to_string())
                .unwrap_or_else(|| "no target".to_string())
        ));
    }
    Ok(())
}

/// Make sure the server is up before a request goes to it: reachable,
/// or woken and waited for. `notice` is told each step as it happens.
pub async fn ensure_awake(
    uri: &Uri,
    path: &Path,
    wait: Duration,
    notice: &(dyn Fn(String) + Sync),
) -> Wake {
    if reachable(uri).await {
        return Wake::Reachable;
    }
    let host = uri.host().unwrap_or("the private server").to_string();
    let Some(neighbor) = remembered(path).await else {
        let outcome = Wake::NoNeighbor;
        if let Some(text) = outcome.note(&host) {
            notice(text);
        }
        return outcome;
    };
    if let Err(e) = send(&neighbor).await {
        let outcome = Wake::Unsent {
            neighbor,
            error: e.to_string(),
        };
        if let Some(text) = outcome.note(&host) {
            notice(text);
        }
        return outcome;
    }
    notice(format!(
        "{host} is not answering — sent a wake-up to {} and waiting up to {}s for it",
        neighbor.mac_text(),
        wait.as_secs()
    ));
    let started = std::time::Instant::now();
    while started.elapsed() < wait {
        tokio::time::sleep(RETRY_EVERY.min(wait)).await;
        if reachable(uri).await {
            let outcome = Wake::Woke {
                neighbor,
                waited: started.elapsed(),
            };
            if let Some(text) = outcome.note(&host) {
                notice(text);
            }
            return outcome;
        }
    }
    let outcome = Wake::StillAsleep {
        neighbor,
        waited: started.elapsed(),
    };
    if let Some(text) = outcome.note(&host) {
        notice(text);
    }
    outcome
}

/// The kernel's neighbour table, read over netlink.
///
/// `RTM_GETNEIGH` with `NLM_F_DUMP` and no family asks for every
/// neighbour the kernel knows — the ARP entries and the IPv6 Neighbor
/// Discovery ones in one answer. Each `RTM_NEWNEIGH` in the reply is an
/// `ndmsg` followed by attributes, of which `NDA_DST` is the address and
/// `NDA_LLADDR` the MAC. Entries whose state is failed or incomplete
/// carry no usable MAC and are left out. A raw netlink socket rather
/// than a crate: the message is three fixed structs and a loop, and the
/// crates that wrap it bring an async runtime of their own.
mod netlink {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use anyhow::{bail, Result};

    pub struct Entry {
        pub ip: IpAddr,
        pub mac: [u8; 6],
        pub ifindex: u32,
    }

    const RTM_NEWNEIGH: u16 = 28;
    const RTM_GETNEIGH: u16 = 30;
    const NLMSG_DONE: u16 = 3;
    const NLMSG_ERROR: u16 = 2;
    const NDA_DST: u16 = 1;
    const NDA_LLADDR: u16 = 2;
    /// States whose entry has no MAC worth having.
    const NUD_INCOMPLETE: u16 = 0x01;
    const NUD_FAILED: u16 = 0x20;
    const NUD_NONE: u16 = 0x00;

    /// Every neighbour with a complete link-layer address.
    pub fn neighbours() -> Result<Vec<Entry>> {
        // SAFETY: a plain socket call; the descriptor is owned below and
        // closed on every path out.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                libc::NETLINK_ROUTE,
            )
        };
        if fd < 0 {
            bail!(
                "opening a netlink socket: {}",
                std::io::Error::last_os_error()
            );
        }
        let socket = Socket(fd);
        socket.send(&dump_request())?;
        let mut entries = Vec::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let received = socket.recv(&mut buffer)?;
            match parse_reply(&buffer[..received], &mut entries)? {
                Reply::More => continue,
                Reply::Done => return Ok(entries),
            }
        }
    }

    struct Socket(libc::c_int);

    impl Socket {
        fn send(&self, bytes: &[u8]) -> Result<()> {
            // SAFETY: `bytes` is a live slice for the duration of the call.
            let sent = unsafe { libc::send(self.0, bytes.as_ptr().cast(), bytes.len(), 0) };
            if sent < 0 {
                bail!(
                    "sending the neighbour dump request: {}",
                    std::io::Error::last_os_error()
                );
            }
            Ok(())
        }

        fn recv(&self, buffer: &mut [u8]) -> Result<usize> {
            // SAFETY: `buffer` is a live, writable slice for the call.
            let received =
                unsafe { libc::recv(self.0, buffer.as_mut_ptr().cast(), buffer.len(), 0) };
            if received < 0 {
                bail!(
                    "reading the neighbour dump: {}",
                    std::io::Error::last_os_error()
                );
            }
            Ok(received as usize)
        }
    }

    impl Drop for Socket {
        fn drop(&mut self) {
            // SAFETY: closing the descriptor this struct owns, once.
            unsafe {
                libc::close(self.0);
            }
        }
    }

    /// `nlmsghdr` + `ndmsg`, family unspecified: dump them all.
    fn dump_request() -> Vec<u8> {
        const NLMSG_HDRLEN: usize = 16;
        const NDMSG_LEN: usize = 12;
        let len = (NLMSG_HDRLEN + NDMSG_LEN) as u32;
        let mut out = Vec::with_capacity(len as usize);
        out.extend_from_slice(&len.to_ne_bytes());
        out.extend_from_slice(&RTM_GETNEIGH.to_ne_bytes());
        let flags = (libc::NLM_F_REQUEST | libc::NLM_F_DUMP) as u16;
        out.extend_from_slice(&flags.to_ne_bytes());
        out.extend_from_slice(&1u32.to_ne_bytes()); // seq
        out.extend_from_slice(&0u32.to_ne_bytes()); // pid: the kernel fills it
        out.extend_from_slice(&[0u8; NDMSG_LEN]); // ndmsg, family AF_UNSPEC
        out
    }

    pub enum Reply {
        More,
        Done,
    }

    /// Walk one `recv`'s worth of messages, collecting neighbours.
    pub fn parse_reply(bytes: &[u8], entries: &mut Vec<Entry>) -> Result<Reply> {
        let mut offset = 0;
        while offset + 16 <= bytes.len() {
            let len = u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            let kind = u16::from_ne_bytes(bytes[offset + 4..offset + 6].try_into().unwrap());
            if len < 16 || offset + len > bytes.len() {
                bail!("a malformed netlink message ({len} bytes)");
            }
            let payload = &bytes[offset + 16..offset + len];
            match kind {
                NLMSG_DONE => return Ok(Reply::Done),
                NLMSG_ERROR => {
                    let code = payload
                        .get(..4)
                        .map(|b| i32::from_ne_bytes(b.try_into().unwrap()))
                        .unwrap_or(0);
                    if code != 0 {
                        bail!(
                            "the kernel refused the neighbour dump: {}",
                            std::io::Error::from_raw_os_error(-code)
                        );
                    }
                }
                RTM_NEWNEIGH => {
                    if let Some(entry) = parse_neighbour(payload) {
                        entries.push(entry);
                    }
                }
                _ => {}
            }
            // Messages are 4-byte aligned.
            offset += (len + 3) & !3;
        }
        Ok(Reply::More)
    }

    /// One `ndmsg` and its attributes: an entry when it has both an
    /// address and a complete MAC.
    fn parse_neighbour(payload: &[u8]) -> Option<Entry> {
        if payload.len() < 12 {
            return None;
        }
        let family = payload[0] as i32;
        let ifindex = i32::from_ne_bytes(payload[4..8].try_into().ok()?) as u32;
        let state = u16::from_ne_bytes(payload[8..10].try_into().ok()?);
        if state == NUD_NONE || state & (NUD_INCOMPLETE | NUD_FAILED) != 0 {
            return None;
        }
        let mut ip = None;
        let mut mac = None;
        let mut offset = 12;
        while offset + 4 <= payload.len() {
            let attr_len =
                u16::from_ne_bytes(payload[offset..offset + 2].try_into().ok()?) as usize;
            let attr_type = u16::from_ne_bytes(payload[offset + 2..offset + 4].try_into().ok()?);
            if attr_len < 4 || offset + attr_len > payload.len() {
                return None;
            }
            let value = &payload[offset + 4..offset + attr_len];
            match attr_type & 0x3fff {
                NDA_DST => {
                    ip = match (family, value.len()) {
                        (libc::AF_INET, 4) => {
                            Some(IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(value).ok()?)))
                        }
                        (libc::AF_INET6, 16) => Some(IpAddr::V6(Ipv6Addr::from(
                            <[u8; 16]>::try_from(value).ok()?,
                        ))),
                        _ => None,
                    };
                }
                NDA_LLADDR if value.len() == 6 => {
                    let bytes = <[u8; 6]>::try_from(value).ok()?;
                    if bytes != [0; 6] {
                        mac = Some(bytes);
                    }
                }
                _ => {}
            }
            offset += (attr_len + 3) & !3;
        }
        Some(Entry {
            ip: ip?,
            mac: mac?,
            ifindex,
        })
    }

    #[cfg(test)]
    pub fn encode_neighbour(
        family: i32,
        ifindex: u32,
        state: u16,
        ip: &[u8],
        mac: &[u8],
    ) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.push(family as u8);
        payload.extend_from_slice(&[0u8; 3]);
        payload.extend_from_slice(&(ifindex as i32).to_ne_bytes());
        payload.extend_from_slice(&state.to_ne_bytes());
        payload.extend_from_slice(&[0u8; 2]);
        for (kind, value) in [(NDA_DST, ip), (NDA_LLADDR, mac)] {
            let attr_len = (4 + value.len()) as u16;
            payload.extend_from_slice(&attr_len.to_ne_bytes());
            payload.extend_from_slice(&kind.to_ne_bytes());
            payload.extend_from_slice(value);
            while payload.len() % 4 != 0 {
                payload.push(0);
            }
        }
        let len = (16 + payload.len()) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(&len.to_ne_bytes());
        out.extend_from_slice(&RTM_NEWNEIGH.to_ne_bytes());
        out.extend_from_slice(&0u16.to_ne_bytes());
        out.extend_from_slice(&1u32.to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes());
        out.extend_from_slice(&payload);
        out
    }

    #[cfg(test)]
    pub fn encode_done() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&20u32.to_ne_bytes());
        out.extend_from_slice(&NLMSG_DONE.to_ne_bytes());
        out.extend_from_slice(&0u16.to_ne_bytes());
        out.extend_from_slice(&1u32.to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes());
        out.extend_from_slice(&0i32.to_ne_bytes());
        out
    }
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A netlink dump reply, both families: complete entries come out
    /// with their address, MAC, and interface; incomplete and failed ones
    /// do not; and the DONE message ends the walk.
    #[test]
    fn the_neighbour_dump_gives_complete_entries_of_both_families() {
        let mac = [0x3c, 0x7c, 0x3f, 0xaa, 0xbb, 0xcc];
        let v6 = Ipv6Addr::new(0xfd84, 0xb081, 0x5fdf, 0x7f82, 0x145, 0xa58d, 0x78b3, 0x7e5);
        let mut reply = Vec::new();
        reply.extend(netlink::encode_neighbour(
            libc::AF_INET,
            2,
            0x02,
            &[192, 168, 86, 193],
            &mac,
        ));
        reply.extend(netlink::encode_neighbour(
            libc::AF_INET6,
            2,
            0x40,
            &v6.octets(),
            &mac,
        ));
        // Incomplete (no MAC yet) and failed entries: left out.
        reply.extend(netlink::encode_neighbour(
            libc::AF_INET,
            2,
            0x01,
            &[192, 168, 86, 1],
            &[0; 6],
        ));
        reply.extend(netlink::encode_neighbour(
            libc::AF_INET,
            2,
            0x20,
            &[192, 168, 86, 2],
            &mac,
        ));
        reply.extend(netlink::encode_done());
        let mut entries = Vec::new();
        assert!(matches!(
            netlink::parse_reply(&reply, &mut entries).unwrap(),
            netlink::Reply::Done
        ));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].ip, IpAddr::V4(Ipv4Addr::new(192, 168, 86, 193)));
        assert_eq!(entries[0].mac, mac);
        assert_eq!(entries[0].ifindex, 2);
        assert_eq!(entries[1].ip, IpAddr::V6(v6));
        assert_eq!(entries[1].mac, mac);
        // A reply with no DONE yet asks for more.
        let mut more = Vec::new();
        let partial = netlink::encode_neighbour(libc::AF_INET, 2, 0x02, &[10, 0, 0, 1], &mac);
        assert!(matches!(
            netlink::parse_reply(&partial, &mut more).unwrap(),
            netlink::Reply::More
        ));
        assert_eq!(more.len(), 1);
    }

    /// The real dump runs unprivileged and answers; what it holds depends
    /// on the machine, so only the call is asserted.
    #[test]
    fn the_kernels_neighbour_table_can_be_read() {
        netlink::neighbours().expect("an unprivileged netlink dump");
    }

    #[test]
    fn the_magic_packet_is_six_ff_then_the_mac_sixteen_times() {
        let mac = [0x3c, 0x7c, 0x3f, 0xaa, 0xbb, 0xcc];
        let packet = magic_packet(&mac);
        assert_eq!(packet.len(), 102);
        assert!(packet[..6].iter().all(|b| *b == 0xff));
        for copy in 0..16 {
            assert_eq!(&packet[6 + copy * 6..12 + copy * 6], &mac);
        }
    }

    #[test]
    fn the_learned_address_lives_beside_the_private_model_file() {
        let path = wake_path(Path::new("/state/ws/private-model.json"));
        assert_eq!(path, PathBuf::from("/state/ws/private-model-wake.json"));
        let neighbor = Neighbor {
            host: "tower.local".into(),
            ip: IpAddr::V4(Ipv4Addr::new(192, 168, 86, 193)),
            mac: [0x3c, 0x7c, 0x3f, 0xaa, 0xbb, 0xcc],
            ifindex: 2,
            learned: 0,
        };
        // A record from before the interface was kept still reads.
        let old: Neighbor = serde_json::from_str(
            r#"{"host":"tower.local","ip":"192.168.86.193","mac":[60,124,63,170,187,204],"learned":0}"#,
        )
        .unwrap();
        assert_eq!(old.ifindex, 0);
        assert_eq!(old.mac, neighbor.mac);
        assert_eq!(neighbor.mac_text(), "3c:7c:3f:aa:bb:cc");
        let woke = Wake::Woke {
            neighbor: neighbor.clone(),
            waited: Duration::from_secs(23),
        };
        let note = woke.note("tower.local").unwrap();
        assert!(note.contains("woke it (3c:7c:3f:aa:bb:cc)"), "{note}");
        assert!(note.contains("after 23s"), "{note}");
        assert!(Wake::NoNeighbor
            .note("tower.local")
            .unwrap()
            .contains("hardware address"));
        assert_eq!(Wake::Reachable.note("tower.local"), None);
    }

    /// A closed port with nothing learned: not reachable, nothing to
    /// wake, and the sentence says why.
    #[tokio::test]
    async fn an_unreachable_server_with_no_neighbour_cannot_be_woken() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let uri: Uri = format!("http://127.0.0.1:{port}").parse().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = wake_path(&dir.path().join("private-model.json"));
        let said = std::sync::Mutex::new(Vec::new());
        let outcome = ensure_awake(&uri, &path, Duration::from_millis(10), &|text| {
            said.lock().unwrap().push(text)
        })
        .await;
        assert_eq!(outcome, Wake::NoNeighbor);
        assert!(!outcome.reached());
        assert!(said.lock().unwrap()[0].contains("cannot be woken yet"));
    }
}
