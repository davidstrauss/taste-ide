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
//! up with. Only IPv4 neighbours are readable that way; a server reached
//! over IPv6 alone is one this cannot wake, and the message says so.
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
//! the LAN and to the neighbour's own subnet; a server on another network
//! is out of reach of a broadcast, which is the protocol's limit and not
//! this module's.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
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
    pub ip: Ipv4Addr,
    /// The hardware address, as the neighbour table gave it.
    pub mac: [u8; 6],
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
    let table = tokio::fs::read_to_string("/proc/net/arp").await.ok()?;
    let neighbor = addrs.iter().find_map(|addr| match addr.ip() {
        IpAddr::V4(ip) => mac_for(&table, ip).map(|mac| Neighbor {
            host: host.clone(),
            ip,
            mac,
            learned: now_seconds(),
        }),
        IpAddr::V6(_) => None,
    })?;
    if let Ok(bytes) = serde_json::to_vec_pretty(&neighbor) {
        if let Some(dir) = path.parent() {
            let _ = tokio::fs::create_dir_all(dir).await;
        }
        let _ = tokio::fs::write(path, bytes).await;
    }
    Some(neighbor)
}

/// The MAC for `ip` in a `/proc/net/arp` table, when the entry is complete
/// (an incomplete one reads as all zeros).
fn mac_for(table: &str, ip: Ipv4Addr) -> Option<[u8; 6]> {
    let wanted = ip.to_string();
    table.lines().skip(1).find_map(|line| {
        let mut fields = line.split_whitespace();
        let address = fields.next()?;
        if address != wanted {
            return None;
        }
        let _hw_type = fields.next()?;
        let _flags = fields.next()?;
        let mac = parse_mac(fields.next()?)?;
        (mac != [0; 6]).then_some(mac)
    })
}

fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = text.split(':');
    for slot in &mut mac {
        *slot = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

/// The Wake-on-LAN magic packet: six `0xff`, then the MAC sixteen times.
pub fn magic_packet(mac: &[u8; 6]) -> [u8; 102] {
    let mut packet = [0xffu8; 102];
    for copy in 0..16 {
        packet[6 + copy * 6..12 + copy * 6].copy_from_slice(mac);
    }
    packet
}

/// Send the wake-up: the limited broadcast, and the neighbour's own /24
/// broadcast, on the discard port and the two ports WoL tools use.
pub async fn send(neighbor: &Neighbor) -> Result<()> {
    let socket = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .await
        .context("opening a UDP socket for the wake-up")?;
    socket
        .set_broadcast(true)
        .context("allowing broadcast on the wake-up socket")?;
    let packet = magic_packet(&neighbor.mac);
    let octets = neighbor.ip.octets();
    let subnet = Ipv4Addr::new(octets[0], octets[1], octets[2], 255);
    let mut sent = 0;
    let mut last_error = None;
    for target in [Ipv4Addr::BROADCAST, subnet] {
        for port in [9u16, 7, 40000] {
            match socket.send_to(&packet, (target, port)).await {
                Ok(_) => sent += 1,
                Err(e) => last_error = Some(e),
            }
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

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str =
        "IP address       HW type     Flags       HW address            Mask     Device\n\
192.168.86.193   0x1         0x2         3c:7c:3f:aa:bb:cc     *        eno1\n\
192.168.86.1     0x1         0x0         00:00:00:00:00:00     *        eno1\n";

    #[test]
    fn the_neighbour_table_gives_a_complete_mac_and_not_an_incomplete_one() {
        assert_eq!(
            mac_for(TABLE, Ipv4Addr::new(192, 168, 86, 193)),
            Some([0x3c, 0x7c, 0x3f, 0xaa, 0xbb, 0xcc])
        );
        // An incomplete entry is all zeros, and no address to wake.
        assert_eq!(mac_for(TABLE, Ipv4Addr::new(192, 168, 86, 1)), None);
        assert_eq!(mac_for(TABLE, Ipv4Addr::new(10, 0, 0, 1)), None);
        assert_eq!(parse_mac("3c:7c:3f:aa:bb:cc:dd"), None, "seven octets");
        assert_eq!(parse_mac("3c:7c"), None);
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
            ip: Ipv4Addr::new(192, 168, 86, 193),
            mac: [0x3c, 0x7c, 0x3f, 0xaa, 0xbb, 0xcc],
            learned: 0,
        };
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
