//! The environment channel: how a relocated agent reaches the IDE.
//!
//! # Why the direction is inverted
//!
//! Phase 4 relocated the agent into its environment's devcontainer and then
//! hit a wall on every SELinux-enforcing host. The IDE bound its unix
//! sockets — one MCP socket per environment, one auth-proxy socket per
//! workspace — and bind-mounted them into the container. Mounting worked.
//! Dialling did not: a `container_t` process is refused `connectto` on a
//! socket whose listener is the unconfined desktop app, so `connect(2)`
//! returns `EACCES` however the file is labelled. Re-verified live on this
//! host (Fedora 44, `getenforce` → `Enforcing`), including with the `:z`
//! relabel that makes the socket `container_file_t` and readable. The
//! `AgentHosting` probe therefore refused relocation there, which was
//! correct and useless.
//!
//! Two things are permitted, and both were re-verified live:
//!
//! - a `container_t` process may connect to a socket **it** bound, and
//! - the unconfined IDE may connect to a socket a `container_t` process
//!   bound.
//!
//! So the endpoint moves into the container. The IDE `podman exec`s one
//! helper per environment; the helper binds
//! [`taste_core::environment::container_mcp_socket`] and
//! [`taste_core::environment::container_auth_socket`] **inside** the
//! container, and everything in there — the agent's MCP stdio bridge, its
//! auth forwarder — dials a socket bound by its own peer. Container to
//! container. Nothing crosses the SELinux boundary as a socket at all.
//!
//! # Why the bytes ride `podman exec` stdio
//!
//! The helper has to hand what it accepts back to the IDE somehow, and its
//! own stdio is already an IDE-owned pipe: `podman exec -i` is how the
//! relocated agent speaks ACP, so it is a channel this design already
//! trusts and already depends on. Measured byte-exact for 200 KB of random
//! data through `podman exec -i` on this host.
//!
//! The alternative — the container binding a socket in a shared host
//! directory for the IDE to dial (also permitted) — needs a mount, a
//! rendezvous protocol on top of it, and a connection pool to avoid a
//! stall, to arrive at the same place. One less moving part wins, and the
//! IDE now bind-mounts **no** socket into a repo-built container.
//!
//! # Why it multiplexes
//!
//! One exec per connection was measured and rejected: `podman exec` costs
//! ~190 ms on this host. MCP would survive that (one agent, one long-lived
//! connection) but the auth path would not — hyper pools connections and an
//! SSE turn holds one open, so every request would pay it, on the path the
//! user watches token by token. One exec per *environment*, framed, pays it
//! once per container.
//!
//! # Identity is still the channel, not the wire
//!
//! ENVIRONMENTS.md's "the socket is the identity" survives the move intact,
//! and by the same construction. The IDE knows which container it exec'd
//! into, so it knows which environment the far end of that pipe is; the
//! environment id is attached here, at the demux, exactly where it used to
//! be attached at `accept`. Nothing a client sends names an environment,
//! and [`Service`] is a closed set of two codes — a container cannot ask
//! for an environment, only for one of the two things the IDE serves it.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use taste_core::environment::{self, EnvironmentId};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// What the IDE serves down an environment channel.
///
/// Deliberately a closed set of two, and deliberately a code on the wire
/// rather than a name: the container asks for one of the things the IDE
/// offers, and there is no string for it to get creative with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    /// This environment's MCP server — the tools, routed to this
    /// environment because of which channel they arrived on.
    Mcp,
    /// The workspace's auth proxy. The wire carries its own identity (a
    /// per-environment placeholder), so this is the same proxy every
    /// environment reaches; the channel does not have to be the identity
    /// here, and is not.
    Auth,
}

impl Service {
    pub fn code(self) -> u8 {
        match self {
            Service::Mcp => 1,
            Service::Auth => 2,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Service::Mcp),
            2 => Some(Service::Auth),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Service::Mcp => "mcp",
            Service::Auth => "auth",
        }
    }
}

/// One end of a demultiplexed connection, as the IDE's servers see it.
///
/// An in-memory duplex rather than a socket: the byte stream is the whole
/// contract, and both consumers ([`taste-mcp`]'s connection handler and the
/// auth proxy's hyper service) are already generic over it. Its buffer is
/// where backpressure comes from — a consumer that stops reading stops the
/// mux reading the pipe, which stops the helper reading the socket, all the
/// way back to the client.
pub type ChannelStream = tokio::io::DuplexStream;

/// The IDE's side of what a channel carries.
///
/// Implemented in `taste-app`, which is the one crate that can see both the
/// MCP server and the auth proxy. Kept as a trait so `taste-devcontainer`
/// depends on neither.
pub trait ChannelServices: Send + Sync + 'static {
    /// Whether this service is available at all. A proxy switched off with
    /// `TASTE_AUTH_PROXY=0` is not, and the hosting probe must not fail an
    /// environment for a door the IDE never opened.
    fn serves(&self, service: Service) -> bool;

    /// Take one connection an in-container client just made. Called on the
    /// IDE's runtime; implementations spawn and return.
    fn accept(&self, env: &EnvironmentId, service: Service, stream: ChannelStream);
}

/// Buffer per demultiplexed connection, both directions.
///
/// Large enough that an SSE turn never stalls on a slow reader in practice,
/// small enough that a stuck consumer cannot grow the IDE: the mux blocks
/// instead, which is the backpressure this design wants.
const CHANNEL_BUFFER: usize = 256 * 1024;

/// Ceiling on one frame's payload. The helper never writes more (it caps
/// its own reads), so anything larger is a corrupt or hostile stream and
/// the channel dies rather than allocating.
const MAX_FRAME: usize = 64 * 1024;

/// How long the helper gets to bind its sockets and say so.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

// --- framing ---------------------------------------------------------------

/// A frame header: `u32be channel | u8 kind | u32be len`.
pub const HEADER_LEN: usize = 9;

/// Frame kinds. `Open` only ever travels container→IDE: the container's
/// clients are the ones that connect, and the IDE never opens a channel of
/// its own. (Agent-created terminals, the next batch, are `podman exec`s in
/// their own right and want nothing from this pipe.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// A client in the container connected to `service`; `channel` is its
    /// id from here on.
    Open { channel: u32, service: Service },
    /// Payload for a live channel.
    Data { channel: u32, bytes: Vec<u8> },
    /// One end hung up. Sent by both sides.
    Close { channel: u32 },
}

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        let (channel, kind, payload): (u32, u8, &[u8]) = match self {
            Frame::Open { channel, service } => (
                *channel,
                0,
                &SERVICE_CODES[service.code() as usize..service.code() as usize + 1],
            ),
            Frame::Data { channel, bytes } => (*channel, 1, bytes),
            Frame::Close { channel } => (*channel, 2, &[]),
        };
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.extend_from_slice(&channel.to_be_bytes());
        out.push(kind);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// Decode one frame from the front of `buf`, returning it and how many
    /// bytes it consumed. `Ok(None)` means "not a whole frame yet".
    pub fn decode(buf: &[u8]) -> Result<Option<(Frame, usize)>> {
        if buf.len() < HEADER_LEN {
            return Ok(None);
        }
        let channel = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let kind = buf[4];
        let len = u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]) as usize;
        if len > MAX_FRAME {
            bail!("channel frame claims {len} bytes, over the {MAX_FRAME} ceiling");
        }
        if buf.len() < HEADER_LEN + len {
            return Ok(None);
        }
        let payload = &buf[HEADER_LEN..HEADER_LEN + len];
        let frame = match kind {
            0 => {
                let code = *payload
                    .first()
                    .context("an open frame carries a service code")?;
                let service = Service::from_code(code)
                    .with_context(|| format!("no service has code {code}"))?;
                Frame::Open { channel, service }
            }
            1 => Frame::Data {
                channel,
                bytes: payload.to_vec(),
            },
            2 => Frame::Close { channel },
            other => bail!("unknown channel frame kind {other}"),
        };
        Ok(Some((frame, HEADER_LEN + len)))
    }
}

/// Service codes as bytes, so an `Open` frame's one-byte payload can be
/// borrowed rather than allocated. Indexed by the code itself.
const SERVICE_CODES: [u8; 3] = [0, 1, 2];

// --- the in-container helper ----------------------------------------------

/// The helper, as a node program.
///
/// `node` for the same reason every other in-container helper here is node:
/// the image belongs to the repo, and the one interpreter an ACP agent's
/// presence guarantees is the one its adapter is written in. It is also
/// what the hosting probe has already established is present.
///
/// It binds one socket per service, tags each accepted connection with that
/// service, and shuttles everything over its own stdio. stdout carries
/// frames and nothing else; stderr carries readiness and diagnostics, so a
/// helper that cannot bind SAYS so instead of hanging.
///
/// Backpressure is honoured in both directions — `pause()` when the far
/// side's buffer is full, `resume()` on drain — because an SSE turn streams
/// through here and a dropped byte is a corrupted turn.
pub const HELPER: &str = "\
const net=require('net'),fs=require('fs'),path=require('path');\
const dir=process.argv[1],specs=process.argv.slice(2);\
fs.mkdirSync(dir,{recursive:true,mode:0o700});\
const chans=new Map();let next=1;\
const out=process.stdout;\
const frame=(id,kind,payload)=>{\
const h=Buffer.alloc(9);h.writeUInt32BE(id,0);h.writeUInt8(kind,4);\
h.writeUInt32BE(payload.length,5);\
return out.write(Buffer.concat([h,payload]))};\
const paused=[];\
out.on('drain',()=>{while(paused.length)paused.pop().resume()});\
const listen=(spec)=>{\
const [code,name]=spec.split(':');\
const p=path.join(dir,name);try{fs.unlinkSync(p)}catch(e){}\
const srv=net.createServer(c=>{\
const id=next++;chans.set(id,c);\
frame(id,0,Buffer.from([parseInt(code,10)]));\
c.on('data',d=>{\
for(let i=0;i<d.length;i+=65536){\
if(!frame(id,1,d.subarray(i,Math.min(i+65536,d.length)))){c.pause();paused.push(c)}}});\
const gone=()=>{if(chans.delete(id))frame(id,2,Buffer.alloc(0))};\
c.on('close',gone);c.on('error',gone)});\
srv.on('error',e=>{console.error('taste-ide channel: '+p+': '+e.message);process.exit(1)});\
srv.listen(p,()=>{fs.chmodSync(p,0o600)});\
return new Promise(r=>srv.on('listening',r))};\
Promise.all(specs.map(listen)).then(()=>console.error('taste-ide channel ready'));\
let buf=Buffer.alloc(0);\
process.stdin.on('data',d=>{\
buf=Buffer.concat([buf,d]);\
for(;;){\
if(buf.length<9)break;\
const id=buf.readUInt32BE(0),kind=buf.readUInt8(4),len=buf.readUInt32BE(5);\
if(buf.length<9+len)break;\
const payload=buf.subarray(9,9+len);buf=buf.subarray(9+len);\
const c=chans.get(id);if(!c)continue;\
if(kind===1){if(!c.write(Buffer.from(payload))){process.stdin.pause();\
c.once('drain',()=>process.stdin.resume())}}\
else if(kind===2){chans.delete(id);c.end()}}});\
process.stdin.on('end',()=>process.exit(0));";

/// The hosting probe's question, as a node program: can something in this
/// container get a real answer out of the IDE through the channel?
///
/// Lives here rather than in the supervisor so the live test can run the
/// same bytes the probe runs. A probe tested by a paraphrase of itself
/// proves nothing about the probe.
///
/// Not "is the socket there" — the helper just bound it, so of course it
/// is. Each service is dialled and made to answer as itself: MCP gets a
/// JSON-RPC `ping` and must return a result carrying the id, the auth proxy
/// gets a credential-less request and must return its own 401. Both prove
/// the whole path — helper, framing, demux, the IDE's own server — and
/// neither costs a token or an upstream call.
///
/// `argv[1]` is the MCP endpoint; `argv[2]` is the auth endpoint, omitted
/// when the IDE is not serving one.
pub const REACH_PROBE: &str = "\
const net=require('net');const [mcp,auth]=process.argv.slice(1);\
const deadline=setTimeout(()=>{console.error('no answer within 15s');process.exit(1)},15000);\
const fail=m=>{console.error(m);process.exit(1)};\
const talk=(p,send,want)=>new Promise(res=>{\
const c=net.connect(p);let got='';\
c.on('connect',()=>c.write(send));\
c.on('data',d=>{got+=d.toString();if(got.includes(want)){c.destroy();res()}});\
c.on('error',e=>fail(p+': '+(e.code||e.message)));\
c.on('close',()=>{if(!got.includes(want))fail(p+': answered '+JSON.stringify(got.slice(0,200)))})});\
const jobs=[talk(mcp,JSON.stringify({jsonrpc:'2.0',id:'taste-probe',method:'ping'})+'\\n',\
'taste-probe')];\
if(auth)jobs.push(talk(auth,'GET /v1/models HTTP/1.1\\r\\nHost: probe\\r\\n\\r\\n','HTTP/1.1 401'));\
Promise.all(jobs).then(()=>{clearTimeout(deadline);console.log('reachable');process.exit(0)});";

/// The helper's argv, given the environment whose container it runs in.
///
/// Each service arrives as `<code>:<basename>`, so the helper never has to
/// know what a service means — it tags what it accepts and the IDE decides.
pub fn helper_command(env: &EnvironmentId) -> Vec<String> {
    vec![
        "node".into(),
        "-e".into(),
        HELPER.into(),
        environment::container_channel_dir(env)
            .display()
            .to_string(),
        format!("{}:mcp.sock", Service::Mcp.code()),
        format!("{}:auth.sock", Service::Auth.code()),
    ]
}

// --- the host side --------------------------------------------------------

/// Where one environment's channel endpoints are, inside its container.
///
/// What a relocated spawn needs: the agent's stdio bridge dials the first,
/// its auth forwarder the second.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelPaths {
    pub mcp: PathBuf,
    pub auth: PathBuf,
}

/// A live environment channel. Dropping it kills the helper, which drops
/// every connection riding it.
pub struct EnvChannel {
    env: EnvironmentId,
    paths: ChannelPaths,
    alive: Arc<AtomicBool>,
    _child: ChildGuard,
    _pump: AbortOnDrop,
}

struct ChildGuard(Mutex<tokio::process::Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Ok(mut child) = self.0.lock() {
            let _ = child.start_kill();
        }
    }
}

struct AbortOnDrop(Vec<tokio::task::JoinHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for handle in &self.0 {
            handle.abort();
        }
    }
}

impl EnvChannel {
    /// Which environment this channel speaks for. The far end never says;
    /// this is what the IDE knows because of which container it exec'd into.
    pub fn environment(&self) -> &EnvironmentId {
        &self.env
    }

    /// The in-container endpoints a relocated spawn points at.
    pub fn paths(&self) -> &ChannelPaths {
        &self.paths
    }

    /// Whether the helper is still running. A channel whose helper died —
    /// the container stopped, or something killed it — answers `false`, and
    /// the supervisor starts a fresh one rather than handing out a dead
    /// address.
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Exec the helper into `container` and start pumping.
    ///
    /// Returns once the helper has bound every socket, so a caller that
    /// hands these paths to an agent knows the agent will find them. A
    /// helper that cannot bind fails here with what it said, rather than
    /// leaving an agent to discover it has no tools.
    /// The substrate is threaded in rather than a `sandboxed` flag because
    /// the channel is the transport everything else rides, and it must
    /// reach the container **wherever that container is**. It is
    /// transport-agnostic by construction — the bytes are the helper's own
    /// stdio — so a machine or a remote host costs it nothing but the
    /// connection flag, which is the property `crates/taste-devcontainer/
    /// tests/substrate.rs` asserts against a real non-local connection.
    pub async fn start(
        env: EnvironmentId,
        container: &str,
        substrate: &crate::substrate::Substrate,
        services: Arc<dyn ChannelServices>,
    ) -> Result<Arc<Self>> {
        let mut args: Vec<String> = vec!["exec".into(), "-i".into(), container.to_string()];
        args.extend(helper_command(&env));
        let mut child = crate::reconcile::podman(substrate, &args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("exec'ing the environment channel helper")?;

        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");

        // Readiness rides stderr because stdout is frames and nothing else.
        let mut lines = BufReader::new(stderr).lines();
        let mut said = Vec::new();
        let ready = tokio::time::timeout(READY_TIMEOUT, async {
            while let Ok(Some(line)) = lines.next_line().await {
                if line.contains("channel ready") {
                    return true;
                }
                said.push(line);
            }
            false
        })
        .await;
        if !matches!(ready, Ok(true)) {
            let _ = child.start_kill();
            let said = if said.is_empty() {
                "it said nothing".to_string()
            } else {
                said.join("; ")
            };
            bail!("the environment channel helper never bound its sockets ({said})");
        }

        let alive = Arc::new(AtomicBool::new(true));
        // Whatever it says from here on is a diagnostic; keep draining so a
        // chatty helper cannot fill a pipe and wedge itself.
        let noisy = tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!("environment channel helper: {line}");
            }
        });

        let (writes_tx, writes_rx) = mpsc::channel::<Vec<u8>>(64);
        let writer = tokio::spawn(write_loop(stdin, writes_rx));
        let pump = tokio::spawn(read_loop(
            stdout,
            env.clone(),
            services,
            writes_tx,
            alive.clone(),
        ));

        Ok(Arc::new(Self {
            paths: ChannelPaths {
                mcp: environment::container_mcp_socket(&env),
                auth: environment::container_auth_socket(&env),
            },
            env,
            alive,
            _child: ChildGuard(Mutex::new(child)),
            _pump: AbortOnDrop(vec![pump, writer, noisy]),
        }))
    }
}

/// Everything the IDE wants to send the helper, serialized through one
/// task: concurrent channels must never interleave halves of a frame.
async fn write_loop(mut stdin: tokio::process::ChildStdin, mut rx: mpsc::Receiver<Vec<u8>>) {
    while let Some(bytes) = rx.recv().await {
        if stdin.write_all(&bytes).await.is_err() {
            break;
        }
    }
}

/// Read frames off the helper's stdout, opening, feeding and closing
/// connections as they arrive.
async fn read_loop(
    mut stdout: tokio::process::ChildStdout,
    env: EnvironmentId,
    services: Arc<dyn ChannelServices>,
    writes: mpsc::Sender<Vec<u8>>,
    alive: Arc<AtomicBool>,
) {
    use std::collections::HashMap;

    // The IDE's half of each live connection: what we write client-bound
    // bytes into.
    let mut open: HashMap<u32, tokio::io::WriteHalf<ChannelStream>> = HashMap::new();
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 32 * 1024];

    loop {
        let read = match stdout.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..read]);
        loop {
            let decoded = match Frame::decode(&buf) {
                Ok(Some(decoded)) => decoded,
                Ok(None) => break,
                Err(e) => {
                    tracing::error!("environment channel {env}: {e}");
                    alive.store(false, Ordering::SeqCst);
                    return;
                }
            };
            let (frame, used) = decoded;
            buf.drain(..used);
            match frame {
                Frame::Open { channel, service } => {
                    if !services.serves(service) {
                        let _ = writes.send(Frame::Close { channel }.encode()).await;
                        continue;
                    }
                    let (ours, theirs) = tokio::io::duplex(CHANNEL_BUFFER);
                    let (read_half, write_half) = tokio::io::split(ours);
                    open.insert(channel, write_half);
                    // The environment is attached HERE — from which channel
                    // this is, never from anything the client said.
                    services.accept(&env, service, theirs);
                    tokio::spawn(client_bound(channel, read_half, writes.clone()));
                }
                Frame::Data { channel, bytes } => {
                    if let Some(write_half) = open.get_mut(&channel) {
                        if write_half.write_all(&bytes).await.is_err() {
                            open.remove(&channel);
                            let _ = writes.send(Frame::Close { channel }.encode()).await;
                        }
                    }
                }
                Frame::Close { channel } => {
                    open.remove(&channel);
                }
            }
        }
    }
    alive.store(false, Ordering::SeqCst);
}

/// One live connection's IDE→container direction: whatever the IDE's server
/// writes becomes data frames, and its hang-up becomes a close.
async fn client_bound(
    channel: u32,
    mut read_half: tokio::io::ReadHalf<ChannelStream>,
    writes: mpsc::Sender<Vec<u8>>,
) {
    let mut chunk = vec![0u8; 32 * 1024];
    loop {
        match read_half.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let frame = Frame::Data {
                    channel,
                    bytes: chunk[..n].to_vec(),
                };
                if writes.send(frame.encode()).await.is_err() {
                    return;
                }
            }
        }
    }
    let _ = writes.send(Frame::Close { channel }.encode()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_survives_its_own_encoding() {
        let frames = [
            Frame::Open {
                channel: 1,
                service: Service::Mcp,
            },
            Frame::Open {
                channel: 4_000_000_000,
                service: Service::Auth,
            },
            Frame::Data {
                channel: 7,
                bytes: b"{\"jsonrpc\":\"2.0\"}\n".to_vec(),
            },
            Frame::Data {
                channel: 7,
                bytes: Vec::new(),
            },
            Frame::Close { channel: 9 },
        ];
        for frame in frames {
            let encoded = frame.encode();
            let (decoded, used) = Frame::decode(&encoded).unwrap().unwrap();
            assert_eq!(decoded, frame);
            assert_eq!(used, encoded.len(), "{frame:?} left bytes behind");
        }
    }

    /// The stream is a stream: frames arrive split and glued, and the
    /// decoder has to want more rather than guess.
    #[test]
    fn a_partial_frame_is_not_a_frame() {
        let frame = Frame::Data {
            channel: 3,
            bytes: b"hello".to_vec(),
        };
        let encoded = frame.encode();
        for cut in 0..encoded.len() {
            assert_eq!(
                Frame::decode(&encoded[..cut]).unwrap(),
                None,
                "decoded a frame from {cut} of {} bytes",
                encoded.len()
            );
        }
        // ...and two glued together come back one at a time.
        let mut both = encoded.clone();
        both.extend_from_slice(&Frame::Close { channel: 3 }.encode());
        let (first, used) = Frame::decode(&both).unwrap().unwrap();
        assert_eq!(first, frame);
        let (second, _) = Frame::decode(&both[used..]).unwrap().unwrap();
        assert_eq!(second, Frame::Close { channel: 3 });
    }

    /// A corrupt or hostile length must not become an allocation, and an
    /// unknown service must not become a connection.
    #[test]
    fn nonsense_is_refused_rather_than_allocated() {
        let mut huge = vec![0u8; HEADER_LEN];
        huge[4] = 1;
        huge[5..9].copy_from_slice(&(MAX_FRAME as u32 + 1).to_be_bytes());
        assert!(Frame::decode(&huge).is_err());

        let mut unknown = vec![0u8; HEADER_LEN + 1];
        unknown[4] = 0; // open
        unknown[8] = 1; // len 1
        unknown[9] = 99; // no such service
        assert!(Frame::decode(&unknown).is_err());

        let mut kind = vec![0u8; HEADER_LEN];
        kind[4] = 77;
        assert!(Frame::decode(&kind).is_err());
    }

    /// The service codes are the wire. Changing one silently re-points a
    /// container's connections at the other service.
    #[test]
    fn the_service_codes_are_fixed() {
        assert_eq!(Service::Mcp.code(), 1);
        assert_eq!(Service::Auth.code(), 2);
        assert_eq!(Service::from_code(1), Some(Service::Mcp));
        assert_eq!(Service::from_code(2), Some(Service::Auth));
        assert_eq!(Service::from_code(0), None);
        assert_eq!(Service::from_code(3), None);
    }

    /// The helper's argv is the whole of what the container is told: where
    /// to bind, and which code goes with which name. The IDE side decodes
    /// the code, so the two must agree here or connections land on the
    /// wrong server.
    #[test]
    fn the_helper_is_told_where_to_bind_and_what_to_call_it() {
        let env = EnvironmentId::parse("review").unwrap();
        let argv = helper_command(&env);
        assert_eq!(argv[0], "node");
        assert_eq!(argv[1], "-e");
        let dir = environment::container_channel_dir(&env);
        assert_eq!(argv[3], dir.display().to_string());
        assert_eq!(argv[4], format!("{}:mcp.sock", Service::Mcp.code()));
        assert_eq!(argv[5], format!("{}:auth.sock", Service::Auth.code()));
        assert_eq!(
            environment::container_mcp_socket(&env),
            dir.join("mcp.sock"),
            "the helper binds what a relocated agent is told to dial"
        );
        assert_eq!(
            environment::container_auth_socket(&env),
            dir.join("auth.sock")
        );
    }

    /// Properties of the helper that are load-bearing and easy to lose in
    /// an edit: it must frame with the same header the Rust side decodes,
    /// respect backpressure both ways, and say when it cannot bind.
    #[test]
    fn the_helper_frames_and_breathes() {
        let js = HELPER;
        // Same 9-byte header, same field order.
        assert!(js.contains("Buffer.alloc(9)"), "{js}");
        assert!(js.contains("h.writeUInt32BE(id,0)"), "{js}");
        assert!(js.contains("h.writeUInt8(kind,4)"), "{js}");
        assert!(js.contains("h.writeUInt32BE(payload.length,5)"), "{js}");
        // Backpressure in both directions: a stalled reader must pause a
        // socket, not drop bytes or grow forever.
        assert!(js.contains("c.pause()") && js.contains("resume()"), "{js}");
        assert!(js.contains("process.stdin.pause()"), "{js}");
        // Never write a payload bigger than the Rust side will decode.
        assert!(js.contains("65536"), "{js}");
        // A helper that cannot bind SAYS so and dies, rather than hanging.
        assert!(
            js.contains("console.error") && js.contains("process.exit(1)"),
            "{js}"
        );
        assert!(js.contains("channel ready"), "{js}");
        // Its own sockets, its own directory, its own permissions.
        assert!(js.contains("mode:0o700"), "{js}");
        assert!(js.contains("chmodSync(p,0o600)"), "{js}");
        assert!(!js.contains("socat"), "{js}");
    }
}
