//! **The keeper: the files service for checkouts that live in a VM.**
//!
//! A checkout in a VM cannot be opened from this host — by decision, since
//! a VM shares no filesystem with it (docs/ENVIRONMENTS.md → "The
//! topology"). What the host can do is talk to a process that has the
//! files. The keeper is that process: one container per VM, built from the
//! IDE's own baseline image, mounting the workspace's directory in the
//! guest, running a small node program that answers file requests over
//! its own stdio. The IDE `podman exec`s into it once and multiplexes
//! every request over that one pipe, which is the transport the
//! environment channel already rides and for the same reason: one exec
//! per VM rather than one per read, because an exec through a connection
//! costs about four hundred milliseconds and a file tree has thousands of
//! entries.
//!
//! # Why a container and not the environment's own
//!
//! The environment's container is the project's: it is rebuilt when the
//! config changes, stopped when idle, and gone when the config is broken.
//! The files are none of those things. A keeper that lived in it would
//! lose the editor's view of the checkout on every rebuild. The keeper's
//! container is the IDE's — the baseline image, which is the rung that
//! always works — mounts every checkout of the workspace on that VM, and
//! outlives any environment's container.
//!
//! # Why node
//!
//! The same reason the channel helper is node: the baseline image carries
//! it because every ACP adapter is a node program, so it is the one
//! interpreter a container the IDE starts is guaranteed to have. Nothing
//! is compiled for the guest and nothing is copied into it.
//!
//! # The protocol
//!
//! Frames both ways, `u32be id | u8 kind | u32be len | payload`, like the
//! channel's. Host to keeper: `0` a JSON request, `1` data for an open
//! write, `2` end — a write's last byte, a watch cancelled, a process
//! killed. Keeper to host: `0` done (JSON, final), `1` data — a read's
//! bytes, a process's stdout — `2` an error (JSON `{code, message}`,
//! final), `3` an event (a watch's, not final), `4` a process's stderr.
//! One id per request, chosen by the host; the keeper answers on it and
//! nothing else.
//!
//! # Blocking on purpose
//!
//! [`Keeper`] implements [`taste_core::files::RemoteFiles`], whose methods
//! block; the IDE side is two plain threads (one writing the pipe, one
//! reading it) and no runtime at all, so a caller on a `spawn_blocking`
//! thread, a test, or a GTK-adjacent worker can all use it the same way.
//! The main thread may not — the rule for `std::fs` applies unchanged.

use std::collections::HashMap;
use std::io::{self, Read as _, Write as _};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use taste_core::files::{Entry, ExecOutput, Kind, RemoteFiles, Stat};

/// The keeper program. Readable rather than minified: it is small, it is
/// the whole of what runs in the guest on the IDE's behalf, and a person
/// debugging a file that would not save deserves to be able to read it.
///
/// One ordering rule inside it: a request that opens something the next
/// frames feed — `write` — registers it **synchronously**, before any
/// `await`. The host sends the request and its data back to back, and an
/// `await` before the registration would let the data arrive first and be
/// dropped, leaving the request to time out. (It did, once.) An end frame
/// for an id with nothing open is answered with an error for the same
/// reason: silence is a ten-minute wait.
///
/// And one about node itself: a `Writable` never emits `drain` once
/// `end()` has been called, so stdin paused for a write's backpressure is
/// resumed by the end frame's handler, never by waiting for a drain that
/// will not come. Left paused, node found nothing else to wait on and
/// exited — cleanly, with code 0, mid-conversation. An interval that
/// never fires keeps the loop alive whatever else is paused.
pub const KEEPER: &str = r#"'use strict';
const fs=require('fs'),fsp=fs.promises,path=require('path'),cp=require('child_process');
const out=process.stdout,CHUNK=65536;
const frame=(id,kind,payload)=>{const h=Buffer.alloc(9);h.writeUInt32BE(id,0);h.writeUInt8(kind,4);h.writeUInt32BE(payload.length,5);return out.write(Buffer.concat([h,payload]))};
const json=(id,kind,obj)=>frame(id,kind,Buffer.from(JSON.stringify(obj)));
const done=(id,obj)=>json(id,0,obj||{});
const fail=(id,e)=>json(id,2,{code:(e&&e.code)||'EIO',message:String((e&&e.message)||e)});
const kindOf=(d)=>d.isFile()?'file':d.isDirectory()?'dir':d.isSymbolicLink()?'symlink':'other';
const writes=new Map(),watches=new Map(),procs=new Map();
const stdinResume=()=>{if(process.stdin.isPaused())process.stdin.resume()};
setInterval(()=>{},2147483647);
const pushFrom=(id,kind,src)=>{src.on('data',d=>{for(let i=0;i<d.length;i+=CHUNK){if(!frame(id,kind,d.subarray(i,Math.min(i+CHUNK,d.length)))){src.pause();out.once('drain',()=>src.resume())}}})};
async function handle(id,req){
  switch(req.op){
    case 'ping':return done(id,{pong:true});
    case 'stat':{const st=await fsp.lstat(req.path);return done(id,{kind:kindOf(st),size:st.size,mtime_ms:Math.floor(st.mtimeMs),mode:st.mode&0o7777})}
    case 'list':{const ents=await fsp.readdir(req.path,{withFileTypes:true});return done(id,{entries:ents.map(d=>({name:d.name,kind:kindOf(d)}))})}
    case 'read':{const fh=await fsp.open(req.path,'r');try{const buf=Buffer.alloc(CHUNK);for(;;){const {bytesRead}=await fh.read(buf,0,CHUNK,null);if(!bytesRead)break;if(!frame(id,1,Buffer.from(buf.subarray(0,bytesRead))))await new Promise(r=>out.once('drain',r))}}finally{await fh.close()}return done(id,{})}
    case 'write':{fs.mkdirSync(path.dirname(req.path),{recursive:true});const tmp=req.path+'.taste-part';const stream=fs.createWriteStream(tmp,{mode:req.mode||0o644});stream.on('error',e=>{writes.delete(id);fail(id,e)});writes.set(id,{tmp,dest:req.path,stream});return}
    case 'mkdir':await fsp.mkdir(req.path,{recursive:true});return done(id,{});
    case 'remove':await fsp.rm(req.path,{recursive:!!req.recursive,force:false});return done(id,{});
    case 'rename':await fsp.rename(req.from,req.to);return done(id,{});
    case 'exec':{const child=cp.spawn(req.argv[0],req.argv.slice(1),{cwd:req.cwd,stdio:['ignore','pipe','pipe'],env:Object.assign({},process.env,req.env||{})});procs.set(id,child);pushFrom(id,1,child.stdout);pushFrom(id,4,child.stderr);child.on('error',e=>{procs.delete(id);fail(id,e)});child.on('close',(code,signal)=>{procs.delete(id);done(id,{status:code===null?-1:code,signal:signal||null})});return}
    case 'watch':{const w=fs.watch(req.path,{recursive:true},(event,name)=>json(id,3,{event,name:name==null?null:String(name)}));w.on('error',e=>{watches.delete(id);fail(id,e)});watches.set(id,w);return}
    default:return fail(id,{code:'EINVAL',message:'unknown op '+req.op});
  }
}
let buf=Buffer.alloc(0);
process.stdin.on('data',d=>{
  buf=Buffer.concat([buf,d]);
  for(;;){
    if(buf.length<9)break;
    const id=buf.readUInt32BE(0),kind=buf.readUInt8(4),len=buf.readUInt32BE(5);
    if(buf.length<9+len)break;
    const payload=Buffer.from(buf.subarray(9,9+len));buf=buf.subarray(9+len);
    if(kind===0){let req;try{req=JSON.parse(payload.toString('utf8'))}catch(e){fail(id,{code:'EINVAL',message:'bad request'});continue}handle(id,req).catch(e=>fail(id,e))}
    else if(kind===1){const w=writes.get(id);if(w&&!w.stream.write(payload)){process.stdin.pause();w.stream.once('drain',stdinResume)}}
    else if(kind===2){
      const w=writes.get(id);if(w){writes.delete(id);stdinResume();w.stream.end(()=>fs.rename(w.tmp,w.dest,e=>e?fail(id,e):done(id,{})));continue}
      const wt=watches.get(id);if(wt){watches.delete(id);wt.close();done(id,{});continue}
      const p=procs.get(id);if(p){p.kill('SIGKILL');continue}
      fail(id,{code:'EINVAL',message:'nothing open on this id'});
    }
  }
});
process.stdin.on('end',()=>process.exit(0));
process.on('uncaughtException',e=>console.error('taste-ide keeper: uncaught',e&&e.stack||e));process.on('unhandledRejection',e=>console.error('taste-ide keeper: unhandled',e&&e.stack||e));
console.error('taste-ide keeper ready');
"#;

/// The keeper's readiness line on stderr.
const READY: &str = "taste-ide keeper ready";
/// How long the keeper gets to say it is ready.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long one request may take before the caller is told the keeper is
/// not answering. Long, because an `exec` may be a `git push` of a
/// repository or an `rg` over one.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
/// Payload per data frame, both ways.
const CHUNK: usize = 64 * 1024;
/// A frame larger than this is a corrupt or hostile stream.
const MAX_FRAME: usize = 1024 * 1024;

const HEADER_LEN: usize = 9;

/// One frame off the keeper's stdout.
#[derive(Debug)]
enum Reply {
    Done(serde_json::Value),
    Data(Vec<u8>),
    Stderr(Vec<u8>),
    Event(serde_json::Value),
    Error { code: String, message: String },
}

fn encode(id: u32, kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&id.to_be_bytes());
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// One frame as decoded off the pipe, borrowing its payload.
struct FrameRef<'a> {
    id: u32,
    kind: u8,
    payload: &'a [u8],
    /// Bytes the frame occupied, header included.
    used: usize,
}

/// Decode one frame from the front of `buf`. `Ok(None)` means "not a
/// whole frame yet".
fn decode(buf: &[u8]) -> Result<Option<FrameRef<'_>>> {
    if buf.len() < HEADER_LEN {
        return Ok(None);
    }
    let id = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let kind = buf[4];
    let len = u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]) as usize;
    if len > MAX_FRAME {
        bail!("keeper frame claims {len} bytes, over the {MAX_FRAME} ceiling");
    }
    if buf.len() < HEADER_LEN + len {
        return Ok(None);
    }
    Ok(Some(FrameRef {
        id,
        kind,
        payload: &buf[HEADER_LEN..HEADER_LEN + len],
        used: HEADER_LEN + len,
    }))
}

/// A live keeper: the process, the pipe, and the requests in flight.
#[derive(Debug)]
pub struct Keeper {
    describe: String,
    writes: Mutex<mpsc::Sender<Vec<u8>>>,
    pending: Arc<Mutex<HashMap<u32, mpsc::Sender<Reply>>>>,
    next_id: AtomicU32,
    alive: Arc<AtomicBool>,
    child: Mutex<Child>,
}

impl Drop for Keeper {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Keeper {
    /// Exec the keeper into `container` on `substrate` and wait for it to
    /// say it is ready.
    pub fn in_container(
        substrate: &crate::substrate::Substrate,
        container: &str,
        describe: impl Into<String>,
    ) -> Result<Arc<Self>> {
        let command = substrate.std_command(&[
            "exec".into(),
            "-i".into(),
            container.into(),
            "node".into(),
            "-e".into(),
            KEEPER.into(),
        ]);
        Self::start(command, describe.into())
    }

    /// The keeper program run directly by this machine's `node`, over this
    /// machine's filesystem. What the tests use: the protocol end to end,
    /// with no podman and no VM.
    #[doc(hidden)]
    pub fn local_node_for_tests() -> Result<Arc<Self>> {
        let mut command = Command::new("node");
        command.args(["-e", KEEPER]);
        Self::start(command, "a local node keeper (test)".into())
    }

    fn start(mut command: Command, describe: String) -> Result<Arc<Self>> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting the keeper for {describe}"))?;
        let mut stdin = child.stdin.take().expect("piped");
        let mut stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");

        // Readiness rides stderr, because stdout is frames and nothing
        // else. Everything after the ready line is a diagnostic.
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let stderr_describe = describe.clone();
        std::thread::Builder::new()
            .name("taste-keeper-stderr".into())
            .spawn(move || {
                let reader = io::BufReader::new(stderr);
                let mut said = Vec::new();
                let mut ready = false;
                for line in io::BufRead::lines(reader).map_while(Result::ok) {
                    if !ready && line.contains(READY) {
                        ready = true;
                        let _ = ready_tx.send(Ok(()));
                        continue;
                    }
                    if ready {
                        tracing::warn!("keeper ({stderr_describe}): {line}");
                    } else {
                        said.push(line);
                    }
                }
                if !ready {
                    let _ = ready_tx.send(Err(if said.is_empty() {
                        "it said nothing".to_string()
                    } else {
                        said.join("; ")
                    }));
                }
            })
            .context("spawning the keeper's stderr thread")?;
        match ready_rx.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(said)) => {
                let _ = child.kill();
                bail!("the keeper for {describe} did not start ({said})");
            }
            Err(_) => {
                let _ = child.kill();
                bail!(
                    "the keeper for {describe} did not say it was ready within {}s",
                    READY_TIMEOUT.as_secs()
                );
            }
        }

        let alive = Arc::new(AtomicBool::new(true));
        let pending: Arc<Mutex<HashMap<u32, mpsc::Sender<Reply>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // The writer: one thread, so two requests never interleave halves
        // of a frame.
        let (writes_tx, writes_rx) = mpsc::channel::<Vec<u8>>();
        std::thread::Builder::new()
            .name("taste-keeper-write".into())
            .spawn(move || {
                for bytes in writes_rx {
                    if stdin.write_all(&bytes).is_err() {
                        break;
                    }
                }
            })
            .context("spawning the keeper's writer thread")?;

        // The reader: frames off stdout, to whoever asked. EOF is the
        // keeper gone, and every waiter finds out at once because its
        // sender is dropped.
        let read_pending = pending.clone();
        let read_alive = alive.clone();
        let read_describe = describe.clone();
        std::thread::Builder::new()
            .name("taste-keeper-read".into())
            .spawn(move || {
                let mut buf: Vec<u8> = Vec::new();
                let mut chunk = vec![0u8; CHUNK];
                loop {
                    let n = match stdout.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    loop {
                        let FrameRef {
                            id,
                            kind,
                            payload,
                            used,
                        } = match decode(&buf) {
                            Ok(Some(frame)) => frame,
                            Ok(None) => break,
                            Err(e) => {
                                tracing::error!("keeper ({read_describe}): {e}");
                                read_alive.store(false, Ordering::SeqCst);
                                read_pending.lock().unwrap().clear();
                                return;
                            }
                        };
                        let reply = match kind {
                            0 => Reply::Done(
                                serde_json::from_slice(payload).unwrap_or(serde_json::Value::Null),
                            ),
                            1 => Reply::Data(payload.to_vec()),
                            2 => {
                                let value: serde_json::Value =
                                    serde_json::from_slice(payload).unwrap_or_default();
                                Reply::Error {
                                    code: value["code"].as_str().unwrap_or("EIO").to_string(),
                                    message: value["message"]
                                        .as_str()
                                        .unwrap_or("the keeper reported an error")
                                        .to_string(),
                                }
                            }
                            3 => Reply::Event(
                                serde_json::from_slice(payload).unwrap_or(serde_json::Value::Null),
                            ),
                            4 => Reply::Stderr(payload.to_vec()),
                            other => {
                                tracing::error!(
                                    "keeper ({read_describe}): unknown frame kind {other}"
                                );
                                read_alive.store(false, Ordering::SeqCst);
                                read_pending.lock().unwrap().clear();
                                return;
                            }
                        };
                        let final_frame = matches!(reply, Reply::Done(_) | Reply::Error { .. });
                        buf.drain(..used);
                        let mut pending = read_pending.lock().unwrap();
                        if final_frame {
                            if let Some(tx) = pending.remove(&id) {
                                let _ = tx.send(reply);
                            }
                        } else if let Some(tx) = pending.get(&id) {
                            let _ = tx.send(reply);
                        }
                    }
                }
                read_alive.store(false, Ordering::SeqCst);
                read_pending.lock().unwrap().clear();
            })
            .context("spawning the keeper's reader thread")?;

        Ok(Arc::new(Self {
            describe,
            writes: Mutex::new(writes_tx),
            pending,
            next_id: AtomicU32::new(1),
            alive,
            child: Mutex::new(child),
        }))
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn gone(&self) -> io::Error {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            format!("the keeper for {} is gone", self.describe),
        )
    }

    fn send(&self, bytes: Vec<u8>) -> io::Result<()> {
        if !self.alive() {
            return Err(self.gone());
        }
        self.writes
            .lock()
            .unwrap()
            .send(bytes)
            .map_err(|_| self.gone())
    }

    /// Send a request and return the receiver its frames arrive on.
    fn request(&self, req: serde_json::Value) -> io::Result<(u32, mpsc::Receiver<Reply>)> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let payload = serde_json::to_vec(&req).map_err(io::Error::other)?;
        if let Err(e) = self.send(encode(id, 0, &payload)) {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        Ok((id, rx))
    }

    /// Drain a request's frames to its final one.
    fn collect(&self, rx: &mpsc::Receiver<Reply>) -> io::Result<(serde_json::Value, ExecOutput)> {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        loop {
            match rx.recv_timeout(REQUEST_TIMEOUT) {
                Ok(Reply::Data(bytes)) => stdout.extend_from_slice(&bytes),
                Ok(Reply::Stderr(bytes)) => stderr.extend_from_slice(&bytes),
                // A watch's event on a request that is not a watch: nothing
                // to do with it but note it. Watches (the tree's, in the
                // batch that moves the primary) drain their own receiver.
                Ok(Reply::Event(event)) => {
                    tracing::trace!("keeper ({}): event {event}", self.describe)
                }
                Ok(Reply::Done(value)) => {
                    let status = value["status"].as_i64().unwrap_or(0) as i32;
                    return Ok((
                        value,
                        ExecOutput {
                            status,
                            stdout,
                            stderr,
                        },
                    ));
                }
                Ok(Reply::Error { code, message }) => return Err(map_error(&code, &message)),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "the keeper for {} did not answer within {}s",
                            self.describe,
                            REQUEST_TIMEOUT.as_secs()
                        ),
                    ))
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(self.gone()),
            }
        }
    }

    fn call(&self, req: serde_json::Value) -> io::Result<(serde_json::Value, ExecOutput)> {
        let (_, rx) = self.request(req)?;
        self.collect(&rx)
    }

    /// `ping`, for the tests and the hosting probe.
    pub fn ping(&self) -> io::Result<()> {
        let (value, _) = self.call(serde_json::json!({ "op": "ping" }))?;
        if value["pong"] == true {
            Ok(())
        } else {
            Err(io::Error::other("the keeper did not pong"))
        }
    }

    /// Run a program beside the files with extra environment variables —
    /// what `git` over ssh needs for `GIT_SSH_COMMAND`, and what the
    /// [`RemoteFiles::exec`] surface has no room for.
    pub fn exec_with_env(
        &self,
        cwd: &Path,
        argv: &[String],
        env: &[(String, String)],
    ) -> io::Result<ExecOutput> {
        let env: serde_json::Map<String, serde_json::Value> = env
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        let (_, output) = self.call(serde_json::json!({
            "op": "exec",
            "cwd": cwd,
            "argv": argv,
            "env": env,
        }))?;
        Ok(output)
    }
}

/// A watch on a directory in the keeper's world. Dropping it ends the
/// watch over there.
#[derive(Debug)]
pub struct WatchHandle {
    id: u32,
    keeper: std::sync::Weak<Keeper>,
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        if let Some(keeper) = self.keeper.upgrade() {
            let _ = keeper.send(encode(self.id, 2, &[]));
        }
    }
}

impl Keeper {
    /// Watch `path` recursively; `on_event` is called with each changed
    /// name, relative to `path`, on a thread of the watch's own. What a
    /// checkout in a VM has instead of inotify. Ends when the handle is
    /// dropped or the keeper is gone.
    pub fn watch(
        self: &Arc<Self>,
        path: &Path,
        on_event: impl Fn(String, String) + Send + 'static,
    ) -> io::Result<WatchHandle> {
        let (id, rx) = self.request(serde_json::json!({ "op": "watch", "path": path }))?;
        std::thread::Builder::new()
            .name("taste-keeper-watch".into())
            .spawn(move || {
                while let Ok(reply) = rx.recv() {
                    match reply {
                        Reply::Event(event) => {
                            if let Some(name) = event["name"].as_str() {
                                let kind = event["event"].as_str().unwrap_or("change");
                                on_event(kind.to_string(), name.to_string());
                            }
                        }
                        Reply::Done(_) | Reply::Error { .. } => break,
                        Reply::Data(_) | Reply::Stderr(_) => {}
                    }
                }
            })?;
        Ok(WatchHandle {
            id,
            keeper: Arc::downgrade(self),
        })
    }
}

/// The keeper's `errno` names, as `io::ErrorKind`s the callers already
/// branch on: `textfile::load` treats `NotFound` as a new file, and must
/// keep doing so for a file in a VM.
fn map_error(code: &str, message: &str) -> io::Error {
    let kind = match code {
        "ENOENT" => io::ErrorKind::NotFound,
        "EEXIST" => io::ErrorKind::AlreadyExists,
        "EACCES" | "EPERM" => io::ErrorKind::PermissionDenied,
        "EINVAL" => io::ErrorKind::InvalidInput,
        "ENOTDIR" => io::ErrorKind::NotADirectory,
        "EISDIR" => io::ErrorKind::IsADirectory,
        "ENOTEMPTY" => io::ErrorKind::DirectoryNotEmpty,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, format!("{message} ({code})"))
}

impl RemoteFiles for Keeper {
    fn describe(&self) -> String {
        self.describe.clone()
    }

    fn stat(&self, path: &Path) -> io::Result<Stat> {
        let (value, _) = self.call(serde_json::json!({ "op": "stat", "path": path }))?;
        Ok(Stat {
            kind: Kind::from_name(value["kind"].as_str().unwrap_or("other")),
            size: value["size"].as_u64().unwrap_or(0),
            mtime_ms: value["mtime_ms"].as_u64().unwrap_or(0),
            mode: value["mode"].as_u64().unwrap_or(0) as u32,
        })
    }

    fn list(&self, path: &Path) -> io::Result<Vec<Entry>> {
        let (value, _) = self.call(serde_json::json!({ "op": "list", "path": path }))?;
        Ok(value["entries"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .map(|e| Entry {
                        name: e["name"].as_str().unwrap_or_default().to_string(),
                        kind: Kind::from_name(e["kind"].as_str().unwrap_or("other")),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let (_, output) = self.call(serde_json::json!({ "op": "read", "path": path }))?;
        Ok(output.stdout)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let (id, rx) = self.request(serde_json::json!({ "op": "write", "path": path }))?;
        for chunk in bytes.chunks(CHUNK) {
            self.send(encode(id, 1, chunk))?;
        }
        self.send(encode(id, 2, &[]))?;
        self.collect(&rx).map(|_| ())
    }

    fn mkdir_all(&self, path: &Path) -> io::Result<()> {
        self.call(serde_json::json!({ "op": "mkdir", "path": path }))
            .map(|_| ())
    }

    fn remove(&self, path: &Path, recursive: bool) -> io::Result<()> {
        self.call(serde_json::json!({ "op": "remove", "path": path, "recursive": recursive }))
            .map(|_| ())
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.call(serde_json::json!({ "op": "rename", "from": from, "to": to }))
            .map(|_| ())
    }

    fn exec(&self, cwd: &Path, argv: &[String]) -> io::Result<ExecOutput> {
        self.exec_with_env(cwd, argv, &[])
    }
}

// --- the keeper's container ------------------------------------------------

/// The keeper container's name in a VM's podman. One per VM, and a VM
/// serves one workspace, so the name needs nothing but the workspace key.
pub fn container_name(workspace_root: &Path) -> String {
    format!(
        "taste-keeper-{}",
        taste_core::environment::workspace_key(workspace_root)
    )
}

/// Bring the keeper's container up in a VM's podman: the baseline image
/// built over there if it is not yet, the workspace's guest directory
/// made and mounted, the container running `sleep infinity` for the IDE
/// to exec into. Idempotent; returns the container name.
///
/// Blocking, like everything about the keeper: it is called from the
/// registry's `create`, which runs on a blocking thread, and a live test.
pub fn ensure_container(
    substrate: &crate::substrate::Substrate,
    vm: &crate::provision::Vm,
    workspace_root: &Path,
) -> Result<String> {
    let mount = crate::provision::guest_workspace_dir(workspace_root);
    // The directory in the guest, made as core over ssh: podman refuses a
    // bind whose source is missing, and nothing else of ours is in the
    // guest yet to make it.
    let keys = crate::keys::Keys::for_workspace(workspace_root);
    let (program, args) = keys.ssh_argv(
        vm.ssh_port,
        [
            "mkdir".to_string(),
            "-p".into(),
            mount.display().to_string(),
        ],
    );
    let made = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("running ssh to make the workspace directory in the guest")?;
    if !made.status.success() {
        bail!(
            "making {} in {}: {}",
            mount.display(),
            vm.domain,
            String::from_utf8_lossy(&made.stderr).trim()
        );
    }

    let name = container_name(workspace_root);
    let state = podman_capture(
        substrate,
        &[
            "ps".into(),
            "-a".into(),
            "--filter".into(),
            format!("name=^{name}$"),
            "--format".into(),
            "{{.State}}".into(),
        ],
    )?;
    match state.trim() {
        "running" => return Ok(name),
        "" => {}
        _ => {
            // Exists and is not running: start it rather than recreate it,
            // so its mount and image stay what they were.
            podman_capture(substrate, &["start".into(), name.clone()])?;
            return Ok(name);
        }
    }

    let config = crate::baseline::ensure_baseline_config()?;
    let key = taste_core::environment::workspace_key(workspace_root);
    let image = crate::image::ensure_image(substrate, &config, &key)?;
    podman_capture(
        substrate,
        &[
            "run".into(),
            "-d".into(),
            "--name".into(),
            name.clone(),
            "--label".into(),
            format!("{}={key}", taste_core::environment::LABEL_WORKSPACE),
            "--label".into(),
            "taste.keeper=1".into(),
            // core in the guest is uid 1000, and so is the baseline's
            // user: the files the keeper writes are core's, which is what
            // every environment container in the guest expects to find.
            "--userns=keep-id:uid=1000,gid=1000".into(),
            // The shared label, and every environment container in the
            // guest binds its checkout with the same: two containers over
            // one tree cannot each relabel it for themselves.
            "-v".into(),
            format!("{}:{}:z", mount.display(), mount.display()),
            image,
            "sleep".into(),
            "infinity".into(),
        ],
    )
    .context("starting the keeper container")?;
    Ok(name)
}

fn podman_capture(substrate: &crate::substrate::Substrate, args: &[String]) -> Result<String> {
    let output = substrate
        .std_command(args)
        .stdin(Stdio::null())
        .output()
        .context("running podman")?;
    if !output.status.success() {
        bail!(
            "podman {}: {}",
            args.first().map(String::as_str).unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use taste_core::files::Files;

    fn node_present() -> bool {
        Command::new("node")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Frames survive their own encoding, and a frame that claims the
    /// world is refused before it is allocated.
    #[test]
    fn frames_round_trip_and_oversize_is_refused() {
        let bytes = encode(7, 1, b"hello");
        let frame = decode(&bytes).unwrap().unwrap();
        assert_eq!(
            (frame.id, frame.kind, frame.payload, frame.used),
            (7, 1, &b"hello"[..], bytes.len())
        );
        assert!(decode(&bytes[..5]).unwrap().is_none(), "not a whole header");
        assert!(
            decode(&bytes[..bytes.len() - 1]).unwrap().is_none(),
            "not a whole frame"
        );
        let mut huge = encode(1, 1, &[]);
        huge[5..9].copy_from_slice(&((MAX_FRAME as u32) + 1).to_be_bytes());
        assert!(decode(&huge).is_err());
    }

    /// errno names become the kinds callers branch on.
    #[test]
    fn errno_names_map_to_error_kinds() {
        assert_eq!(map_error("ENOENT", "x").kind(), io::ErrorKind::NotFound);
        assert_eq!(
            map_error("EEXIST", "x").kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            map_error("EACCES", "x").kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(map_error("EBOGUS", "x").kind(), io::ErrorKind::Other);
        assert!(map_error("ENOENT", "no such file")
            .to_string()
            .contains("ENOENT"));
    }

    /// The whole protocol, against a real node running the keeper program
    /// over a real directory: every op the remote arm has, including a
    /// write larger than one frame, a read of it back, an exec with both
    /// streams and a non-zero status, and the errors a caller branches on.
    #[test]
    fn the_keeper_answers_every_op_over_its_pipe() {
        if !node_present() {
            eprintln!("SKIP: no node on this machine; the keeper program cannot be run");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let keeper = Keeper::local_node_for_tests().expect("the keeper starts");
        keeper.ping().unwrap();
        let files = Files::Remote(keeper.clone());
        assert!(!files.is_local());

        // A write spanning three frames, read back byte for byte, and
        // renamed into place with no part file left behind.
        let big: Vec<u8> = (0..(2 * CHUNK + 1234)).map(|i| (i % 251) as u8).collect();
        files.write(&root.join("deep/er/blob.bin"), &big).unwrap();
        let on_disk = std::fs::read(root.join("deep/er/blob.bin")).unwrap();
        assert_eq!(on_disk.len(), big.len(), "the write landed whole");
        let back = files.read(&root.join("deep/er/blob.bin")).unwrap();
        let first_diff = back.iter().zip(big.iter()).position(|(a, b)| a != b);
        assert!(
            back.len() == big.len() && first_diff.is_none(),
            "read back {} bytes of {}, first difference at {first_diff:?}",
            back.len(),
            big.len()
        );
        assert!(!root.join("deep/er/blob.bin.taste-part").exists());
        let stat = files.stat(&root.join("deep/er/blob.bin")).unwrap();
        assert_eq!(stat.kind, Kind::File);
        assert_eq!(stat.size, big.len() as u64);
        assert!(stat.mtime_ms > 0);

        files.write(&root.join("deep/a.txt"), b"a").unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("deep/link")).unwrap();
        let listed = files.list(&root.join("deep")).unwrap();
        let names: Vec<(&str, Kind)> = listed.iter().map(|e| (e.name.as_str(), e.kind)).collect();
        assert_eq!(
            names,
            vec![
                ("a.txt", Kind::File),
                ("er", Kind::Dir),
                ("link", Kind::Symlink)
            ]
        );
        assert_eq!(
            files.stat(&root.join("deep/link")).unwrap().kind,
            Kind::Symlink
        );

        files.mkdir_all(&root.join("made/here")).unwrap();
        assert!(files.is_dir(&root.join("made/here")));
        files
            .rename(&root.join("deep/a.txt"), &root.join("made/b.txt"))
            .unwrap();
        assert_eq!(files.read_to_string(&root.join("made/b.txt")).unwrap(), "a");
        assert!(
            files.remove(&root.join("made"), false).is_err(),
            "not empty"
        );
        files.remove(&root.join("made"), true).unwrap();
        assert!(!files.exists(&root.join("made")));

        // exec: both streams, the status, and the cwd.
        let out = files
            .exec(
                root,
                &["sh".into(), "-c".into(), "pwd; echo err >&2; exit 4".into()],
            )
            .unwrap();
        assert_eq!(out.status, 4);
        assert_eq!(
            out.stdout_utf8().trim(),
            root.canonicalize().unwrap().display().to_string()
        );
        assert_eq!(out.stderr_utf8(), "err\n");
        let with_env = keeper
            .exec_with_env(
                root,
                &["sh".into(), "-c".into(), "echo $TASTE_TEST".into()],
                &[("TASTE_TEST".into(), "yes".into())],
            )
            .unwrap();
        assert_eq!(with_env.stdout_utf8(), "yes\n");

        // Errors carry their kind, so `load`'s new-file branch works.
        assert_eq!(
            files.read(&root.join("nope")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            files
                .list(&root.join("deep/er/blob.bin"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotADirectory
        );
        let missing_program = files
            .exec(root, &["/definitely/not/a/program".into()])
            .unwrap_err();
        assert_eq!(missing_program.kind(), io::ErrorKind::NotFound);

        // Many requests in flight at once come back to the right callers.
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let files = files.clone();
                let path = root.join(format!("many/{i}.txt"));
                std::thread::spawn(move || {
                    files.write(&path, format!("file {i}").as_bytes()).unwrap();
                    files.read_to_string(&path).unwrap()
                })
            })
            .collect();
        for (i, handle) in handles.into_iter().enumerate() {
            assert_eq!(handle.join().unwrap(), format!("file {i}"));
        }
        assert!(keeper.alive());
    }

    /// A watch reports what changes under it, by name, and ends when its
    /// handle is dropped.
    #[test]
    fn a_watch_reports_changes_and_ends_with_its_handle() {
        if !node_present() {
            eprintln!("SKIP: no node on this machine");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".devcontainer")).unwrap();
        let keeper = Keeper::local_node_for_tests().unwrap();
        let (tx, rx) = mpsc::channel::<String>();
        let handle = keeper
            .watch(dir.path(), move |_event, name| {
                let _ = tx.send(name);
            })
            .unwrap();
        // The watch is armed asynchronously over there; give it a moment.
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(dir.path().join(".devcontainer/devcontainer.json"), "{}").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut saw = Vec::new();
        while std::time::Instant::now() < deadline {
            if let Ok(name) = rx.recv_timeout(Duration::from_millis(200)) {
                saw.push(name.clone());
                if name.contains("devcontainer.json") {
                    break;
                }
            }
        }
        assert!(
            saw.iter().any(|n| n.contains("devcontainer.json")),
            "the change was reported: {saw:?}"
        );
        drop(handle);
        // The keeper is still there for other requests.
        keeper.ping().unwrap();
    }

    /// A keeper that dies fails every waiter at once, rather than leaving
    /// them to time out one by one.
    #[test]
    fn a_dead_keeper_fails_its_callers_promptly() {
        if !node_present() {
            eprintln!("SKIP: no node on this machine");
            return;
        }
        let keeper = Keeper::local_node_for_tests().unwrap();
        keeper.child.lock().unwrap().kill().unwrap();
        // The reader sees EOF and marks it dead; give it a moment.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while keeper.alive() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!keeper.alive());
        let err = keeper.ping().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe, "{err}");
    }

    #[test]
    fn the_container_is_named_for_the_workspace() {
        let name = container_name(Path::new("/work/proj"));
        assert!(name.starts_with("taste-keeper-"));
        assert!(!name.contains("/"));
    }
}
