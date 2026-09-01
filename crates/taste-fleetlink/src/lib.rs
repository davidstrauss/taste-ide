//! `net.davidstrauss.taste.Fleet` — the IDE's fleet, served over varlink.
//!
//! ENVIRONMENTS.md → "Shell integration rides a varlink interface". The
//! rule, stated there and honoured here: **varlink for interfaces we
//! design; the established contract — D-Bus included — when implementing
//! someone else's.** This is one we designed, so it is varlink; the GNOME
//! search provider, when it lands, is `org.gnome.Shell.SearchProvider2`
//! over D-Bus because that one is GNOME's.
//!
//! **This crate holds no inventory.** It is a socket, a wire format, and a
//! [`Snapshot`] someone else hands it. The fleet is assembled exactly once,
//! in `taste-app`'s `fleet::assemble`, from the six places an
//! environment's facts actually live; gadget mode renders those rows and
//! so does this. A second derivation here is how two surfaces end up
//! disagreeing about what an environment is called, which is the whole
//! reason the row model exists.
//!
//! **Read-only, and not by omission.** Nothing here mutates anything. A
//! process that can open a socket in the user's runtime directory is not
//! thereby entitled to start containers, destroy environments or answer
//! permission prompts; whether anything ever is, is a decision for a later
//! phase and a separately named interface.
//!
//! No GTK, no taste crates: the app converts its rows into [`Row`] and
//! calls [`FleetService::publish`]. Everything below the app is plain
//! serde and tokio.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

pub mod idl;
pub mod protocol;

use protocol::{Call, Reply};

/// The interface this crate serves.
pub const INTERFACE: &str = "net.davidstrauss.taste.Fleet";

/// The read model's version, carried in every reply.
///
/// Not the IDE's version: it is the shape of a [`Row`]. Adding an optional
/// field leaves it alone; removing or retyping one bumps it, and a client
/// that does not recognise the number it sees should render nothing rather
/// than guess.
///
/// **2** added `openIssues`. Strictly that is an addition and could have
/// ridden version 1 — it is declared anyway, because a client rendering a
/// fleet without the issue queue is now rendering half of one, and the
/// number is how it can tell.
///
/// **3** added `workspaceRoot`, and is declared for the same reason with
/// more force. N windows are open at once by design, and until now the only
/// thing a reply said about which one it came from was `workspace`, a bare
/// directory name — so a client aggregating every open project rendered
/// `~/work/api` and `~/archive/api` as one indistinguishable pair of rows.
/// A client that cannot tell two fleets apart is not rendering a fleet, so
/// the number is how it learns it can.
pub const VERSION: u64 = 3;

/// The interface description, verbatim — the same bytes
/// `GetInterfaceDescription` returns.
pub const DESCRIPTION: &str = include_str!("net.davidstrauss.taste.Fleet.varlink");

/// What one environment has spent through the auth proxy.
///
/// `int` in the IDL. There is no limit field and no percentage: the proxy
/// records spend and does not enforce it, and the upstream API exposes no
/// subscription ceiling, so a denominator here would be invented. Clients
/// render burn, not fullness.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Spend {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl std::ops::AddAssign for Spend {
    fn add_assign(&mut self, other: Self) {
        self.requests += other.requests;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

impl Spend {
    pub fn is_zero(&self) -> bool {
        *self == Spend::default()
    }
}

/// The chat working in an environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chat {
    pub label: String,
    pub busy: bool,
    /// This is the workspace's orchestrator chat. At most one row in a
    /// fleet carries it.
    pub orchestrator: bool,
}

/// One environment, on the wire.
///
/// Optional fields serialize as explicit `null` rather than being omitted.
/// Varlink permits either, and a constant JSON shape is kinder to the
/// weakly-typed clients this interface exists for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub environment: String,
    pub name: String,
    pub named: bool,
    pub primary: bool,
    pub mode: String,
    pub state: String,
    pub detail: String,
    pub pending_rebuild: bool,
    pub chat: Option<Chat>,
    pub branch: Option<String>,
    pub unpublished: u64,
    pub dirty: u64,
    /// Whether the git pass has run for this row. While false, `branch`,
    /// `unpublished` and `dirty` are unknown — not zero.
    pub git_known: bool,
    pub published: u64,
    pub shells: u64,
    pub disk_bytes: Option<u64>,
    pub spend: Spend,
}

/// The whole fleet at one instant.
///
/// The aggregates are computed, never stored: `inbox` and the total spend
/// are sums over these rows, so there is exactly one place they can be
/// wrong, and the gadget card and this service read the same one.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Snapshot {
    /// The open workspace's directory name.
    pub workspace: String,
    /// Open issues on `refs/taste/issues`.
    ///
    /// The one number here that is *not* a sum over the rows, and it is
    /// stored rather than derived for a reason: issues belong to the
    /// workspace, not to an environment, and an unclaimed issue belongs to
    /// no environment at all. Deriving it from the rows would mean either
    /// inventing a home for unclaimed work or losing it.
    pub open_issues: u64,
    /// Primary first, then by the name the user reads.
    pub rows: Vec<Row>,
}

impl Snapshot {
    /// Branches waiting in the user's review inbox, across every
    /// environment.
    pub fn inbox(&self) -> u64 {
        self.rows.iter().map(|row| row.published).sum()
    }

    /// The fleet's fuel gauge: every environment's spend, added up.
    pub fn spend(&self) -> Spend {
        let mut total = Spend::default();
        for row in &self.rows {
            total += row.spend;
        }
        total
    }

    /// How many environments are running the project's own configuration —
    /// the working mode, and the one the UI names by saying nothing.
    /// Rendered by the gadget's header and by any indicator.
    pub fn running(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.mode == "container")
            .count()
    }

    /// Chats with a turn in flight right now.
    pub fn busy(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.chat.as_ref().is_some_and(|chat| chat.busy))
            .count()
    }

    /// The reply body, identical for `List` and `Watch` — one shape, so a
    /// client can hand both to the same renderer.
    ///
    /// `workspace_root` comes from the service rather than the snapshot: it
    /// is what the socket answers for, and it does not change while the
    /// window is open.
    pub fn parameters(&self, workspace_root: &str) -> Value {
        json!({
            "version": VERSION,
            "workspace": self.workspace,
            "workspaceRoot": workspace_root,
            "inbox": self.inbox(),
            "spend": self.spend(),
            "openIssues": self.open_issues,
            "rows": self.rows,
        })
    }
}

/// A running fleet service. Clone it freely; every clone publishes into
/// and reads from the same snapshot.
#[derive(Clone)]
pub struct FleetService {
    tx: Arc<watch::Sender<Arc<Snapshot>>>,
    /// The open folder, in full.
    ///
    /// On the service rather than in the [`Snapshot`] because it is the
    /// service's identity and the snapshot's contents are not: the window
    /// republishes the fleet on every environment event, and the folder it
    /// has open is the one fact about it that cannot change while it is
    /// open.
    workspace_root: Arc<str>,
}

impl FleetService {
    /// A service for one open folder.
    ///
    /// The root is the full path, not the directory name. A client with
    /// several sockets in front of it — the shell extension aggregating
    /// every project the user has open — needs to tell `~/work/api` from
    /// `~/archive/api`, and a basename cannot.
    pub fn new(workspace_root: impl Into<Arc<str>>, initial: Snapshot) -> Self {
        let (tx, _rx) = watch::channel(Arc::new(initial));
        Self {
            tx: Arc::new(tx),
            workspace_root: workspace_root.into(),
        }
    }

    /// The folder this service answers for.
    pub fn workspace_root(&self) -> &str {
        &self.workspace_root
    }

    /// Replace the fleet, waking every watcher. A snapshot equal to the
    /// current one wakes nobody — the app republishes on every environment
    /// event, and a watcher woken by an event that changed nothing is a
    /// desktop repaint for no reason.
    ///
    /// Returns whether anything actually moved.
    pub fn publish(&self, snapshot: Snapshot) -> bool {
        self.tx.send_if_modified(|current| {
            if **current == snapshot {
                return false;
            }
            *current = Arc::new(snapshot);
            true
        })
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.tx.borrow().clone()
    }

    /// Bind the socket. Separate from [`FleetService::serve_on`] so a
    /// caller (and a test) knows the socket exists before anything dials
    /// it.
    pub fn bind(socket: &Path) -> Result<UnixListener> {
        if let Some(parent) = socket.parent() {
            let _ = std::fs::create_dir_all(parent);
            // The discovery directory is private. In $XDG_RUNTIME_DIR that
            // is already true; in the /tmp fallback it is this line.
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
        // Whether anything is already answering here decides everything.
        //
        // This used to unlink unconditionally, on the reasoning that a live
        // service "belongs to another window on another workspace and
        // cannot be at this path". True of two windows on two folders —
        // and false in exactly the case that matters, two windows on ONE
        // folder, which derive the same path. The unlink made the second
        // window silently steal the first's socket: the first kept a
        // listener nothing could ever reach again, and every shell
        // extension watching the fleet followed the thief.
        //
        // So: probe first. A socket somebody answers on is refused, and the
        // caller declines to supervise (see `taste_core::instance` — the
        // same window will already have lost the supervision lock, and this
        // is the belt to that pair of braces). A socket nobody answers on is
        // a dead window's leavings and is cleared.
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            anyhow::bail!(
                "a live fleet service is already serving {} — another window has \
                 this folder open",
                socket.display()
            );
        }
        let _ = std::fs::remove_file(socket);
        let listener =
            UnixListener::bind(socket).with_context(|| format!("binding {}", socket.display()))?;
        // The fleet says what the user is working on and what it cost.
        // $XDG_RUNTIME_DIR is already private, but this also has to be
        // right in the /tmp fallback.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("securing {}", socket.display()))?;
        Ok(listener)
    }

    /// Serve until the process ends. One task per connection.
    pub async fn serve(self, socket: std::path::PathBuf) -> Result<()> {
        let listener = Self::bind(&socket)?;
        tracing::info!("fleet service listening on {}", socket.display());
        self.serve_on(listener).await
    }

    pub async fn serve_on(self, listener: UnixListener) -> Result<()> {
        loop {
            let (stream, _addr) = listener.accept().await?;
            let service = self.clone();
            tokio::spawn(async move {
                if let Err(e) = service.handle(stream).await {
                    tracing::debug!("fleet connection ended: {e:#}");
                }
            });
        }
    }

    /// One connection: framed calls in, framed replies out, until the
    /// client hangs up.
    pub async fn handle(self, stream: UnixStream) -> Result<()> {
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let read = reader.read_until(protocol::DELIMITER, &mut buf).await?;
            if read == 0 {
                return Ok(()); // clean hangup
            }
            if buf.last() != Some(&protocol::DELIMITER) {
                anyhow::bail!("message was not NUL-terminated");
            }
            buf.pop();
            let call = match protocol::decode_call(&buf) {
                Ok(call) => call,
                Err(e) => {
                    // Unparseable input gets one honest reply, then the
                    // connection goes: the framing is no longer trusted.
                    let reply = Reply::invalid_parameter("parameters");
                    let _ = write.write_all(&protocol::encode(&reply)).await;
                    anyhow::bail!("undecodable call: {e}");
                }
            };
            match self.answer(&call) {
                // A `Watch` takes the connection over for good: replies
                // keep coming until one side goes away, and nothing else
                // can be interleaved on the same socket.
                Answer::Stream => return self.watch(&mut write).await,
                // `oneway`, and the degenerate `Watch` nobody will read.
                Answer::Silence => continue,
                Answer::Reply(reply) => write.write_all(&protocol::encode(&reply)).await?,
            }
        }
    }

    /// What one call earns — the dispatch, with no IO in it, so the
    /// answers are decided in one readable place.
    fn answer(&self, call: &Call) -> Answer {
        // `upgrade` asks to leave varlink behind for some other protocol
        // on the same socket. There isn't one; say so rather than answer
        // in a protocol the client has stopped speaking.
        if call.upgrade {
            return Answer::for_call(
                call,
                Reply::error(
                    format!("{}.MethodNotImplemented", protocol::SERVICE_INTERFACE),
                    json!({ "method": call.method }),
                ),
            );
        }
        match call.method.as_str() {
            "org.varlink.service.GetInfo" => Answer::for_call(call, Reply::ok(self.get_info())),
            "org.varlink.service.GetInterfaceDescription" => {
                let requested = call
                    .parameters
                    .get("interface")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let reply = match requested {
                    INTERFACE => Reply::ok(json!({ "description": DESCRIPTION })),
                    protocol::SERVICE_INTERFACE => {
                        Reply::ok(json!({ "description": SERVICE_DESCRIPTION }))
                    }
                    "" => Reply::invalid_parameter("interface"),
                    other => Reply::interface_not_found(other),
                };
                Answer::for_call(call, reply)
            }
            "net.davidstrauss.taste.Fleet.List" => Answer::for_call(
                call,
                Reply::ok(self.snapshot().parameters(&self.workspace_root)),
            ),
            "net.davidstrauss.taste.Fleet.Watch" => {
                if !call.more {
                    return Answer::for_call(call, Reply::expected_more());
                }
                if call.oneway {
                    // A stream nobody will read. Drop it rather than hold
                    // a task open forever writing into a void.
                    return Answer::Silence;
                }
                Answer::Stream
            }
            other => {
                let reply = match call.interface() {
                    Some(INTERFACE) | Some(protocol::SERVICE_INTERFACE) | None => {
                        Reply::method_not_found(other)
                    }
                    Some(interface) => Reply::interface_not_found(interface),
                };
                Answer::for_call(call, reply)
            }
        }
    }

    /// The `Watch` stream: the fleet now, then the fleet again on every
    /// change, each reply flagged `continues`.
    ///
    /// The stream has no end. It stops when the client closes the
    /// connection — which is what makes cancellation free — or when the
    /// window does.
    async fn watch(&self, write: &mut (impl tokio::io::AsyncWrite + Unpin)) -> Result<()> {
        let mut rx = self.tx.subscribe();
        loop {
            // `borrow_and_update` marks this value seen, so a change that
            // lands while we are writing wakes us exactly once more.
            let snapshot = rx.borrow_and_update().clone();
            write
                .write_all(&protocol::encode(&Reply::ok_more(
                    snapshot.parameters(&self.workspace_root),
                )))
                .await?;
            if rx.changed().await.is_err() {
                return Ok(()); // the service is gone
            }
        }
    }
}

/// What the connection does with one call.
enum Answer {
    Reply(Reply),
    /// Say nothing and read the next call.
    Silence,
    /// Hand the connection to the `Watch` stream, for good.
    Stream,
}

impl Answer {
    /// A reply, unless the client asked not to be answered.
    fn for_call(call: &Call, reply: Reply) -> Self {
        if call.oneway {
            Answer::Silence
        } else {
            Answer::Reply(reply)
        }
    }
}

impl FleetService {
    /// `org.varlink.service.GetInfo`.
    ///
    /// The product string names the workspace this window has open, which is
    /// how a client holding several sockets at once labels them without
    /// having to call into our own interface first. It used to say a bare
    /// "Taste IDE" while this comment claimed otherwise — harmless with one
    /// window open, and a list of identical rows with four.
    ///
    /// The full root, not the basename: `~/work/api` and `~/archive/api`
    /// are two projects and a person needs to see which is which.
    fn get_info(&self) -> Value {
        json!({
            "vendor": "David Strauss",
            "product": format!("Taste IDE — {}", self.workspace_root),
            "version": env!("CARGO_PKG_VERSION"),
            "url": "https://github.com/davidstrauss/taste-ide",
            "interfaces": [protocol::SERVICE_INTERFACE, INTERFACE],
        })
    }
}

/// The standard service interface, as varlink defines it. Served so a
/// generic client can introspect this socket with no prior knowledge.
const SERVICE_DESCRIPTION: &str = "\
interface org.varlink.service

method GetInfo() -> (
  vendor: string,
  product: string,
  version: string,
  url: string,
  interfaces: []string
)

method GetInterfaceDescription(interface: string) -> (description: string)

error InterfaceNotFound (interface: string)
error MethodNotFound (method: string)
error MethodNotImplemented (method: string)
error InvalidParameter (parameter: string)
error PermissionDenied ()
error ExpectedMore ()
";

#[cfg(test)]
mod tests {
    use super::*;

    /// The folder this window has open, in full — the service's identity.
    const ROOT: &str = "/home/dev/work/project";

    fn row(slug: &str, mode: &str) -> Row {
        Row {
            environment: slug.into(),
            name: slug.into(),
            named: false,
            primary: slug == "primary",
            mode: mode.into(),
            state: if mode == "container" {
                "running".into()
            } else {
                "stopped".into()
            },
            // What `FleetRow::state_text` would have produced: the
            // ordinary case names no mode, and the rung with nothing
            // running is "no environment".
            detail: if mode == "container" {
                "running".into()
            } else {
                "no environment · stopped".to_string()
            },
            pending_rebuild: false,
            chat: None,
            branch: None,
            unpublished: 0,
            dirty: 0,
            git_known: false,
            published: 0,
            shells: 0,
            disk_bytes: None,
            spend: Spend::default(),
        }
    }

    fn fleet() -> Snapshot {
        let mut calm = row("calm-1", "container");
        calm.chat = Some(Chat {
            label: "Claude 2".into(),
            busy: true,
            orchestrator: true,
        });
        calm.published = 2;
        calm.spend = Spend {
            requests: 12,
            input_tokens: 41_000,
            output_tokens: 3_500,
        };
        let mut spry = row("spry-2", "safe");
        spry.published = 1;
        spry.spend = Spend {
            requests: 3,
            input_tokens: 1_000,
            output_tokens: 100,
        };
        Snapshot {
            workspace: "taste-ide".into(),
            open_issues: 4,
            rows: vec![row("primary", "container"), calm, spry],
        }
    }

    // --- the read model --------------------------------------------------

    #[test]
    fn the_aggregates_are_sums_of_the_rows_and_nothing_else() {
        let fleet = fleet();
        assert_eq!(fleet.inbox(), 3, "every environment's published branches");
        assert_eq!(
            fleet.spend(),
            Spend {
                requests: 15,
                input_tokens: 42_000,
                output_tokens: 3_600
            }
        );
        assert_eq!(fleet.running(), 2);
        assert_eq!(fleet.busy(), 1);
        assert_eq!(Snapshot::default().spend(), Spend::default());
        // The one number that is not a sum: the queue is the workspace's,
        // and no row can account for an unclaimed issue.
        assert_eq!(fleet.parameters(ROOT)["openIssues"], 4);
        assert_eq!(fleet.parameters(ROOT)["version"], 3);
        // The window's identity, in full — a basename cannot separate
        // `~/work/api` from `~/archive/api`, and N windows are open at once
        // by design.
        assert_eq!(fleet.parameters(ROOT)["workspaceRoot"], ROOT);
    }

    /// The checked-in IDL is what a client reads to learn the wire. If it
    /// and the structs drift, the interface is a lie — so the fields are
    /// compared, not eyeballed.
    #[test]
    fn the_idl_describes_exactly_what_the_service_serialises() {
        let interface = idl::parse(DESCRIPTION).expect("the checked-in IDL parses");
        assert_eq!(interface.name, INTERFACE);
        assert_eq!(
            idl::parse(&idl::render(&interface)).unwrap(),
            interface,
            "round-trip"
        );

        let keys = |value: &Value| -> Vec<String> {
            value
                .as_object()
                .expect("an object")
                .keys()
                .cloned()
                .collect()
        };
        let sample = serde_json::to_value(row("calm-1", "container")).unwrap();
        let mut declared: Vec<String> = interface
            .type_named("Row")
            .expect("type Row")
            .field_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut serialised = keys(&sample);
        declared.sort();
        serialised.sort();
        assert_eq!(declared, serialised, "Row: IDL vs serde");
        assert!(
            sample.get("chat").is_some_and(Value::is_null),
            "optional fields serialise as null, not as absent keys"
        );

        let mut declared: Vec<String> = interface
            .type_named("Spend")
            .unwrap()
            .field_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut serialised = keys(&serde_json::to_value(Spend::default()).unwrap());
        declared.sort();
        serialised.sort();
        assert_eq!(declared, serialised, "Spend: IDL vs serde");

        // Both methods return the same shape, and it is the shape
        // `parameters()` builds.
        let body = fleet().parameters(ROOT);
        for method in ["List", "Watch"] {
            let method = interface.method_named(method).expect(method);
            assert!(method.parameters.is_empty(), "read-only: no arguments");
            let mut declared: Vec<String> = method
                .results
                .iter()
                .map(|field| field.name.clone())
                .collect();
            let mut serialised = keys(&body);
            declared.sort();
            serialised.sort();
            assert_eq!(declared, serialised);
        }
        assert_eq!(
            interface.qualified_methods(),
            [
                "net.davidstrauss.taste.Fleet.List",
                "net.davidstrauss.taste.Fleet.Watch"
            ],
            "adding a method to a public interface is a deliberate act"
        );
        // The service is read-only by design; a method that sounds like a
        // mutation must not have slipped in.
        for method in interface.qualified_methods() {
            assert!(
                !["Set", "Create", "Destroy", "Start", "Stop", "Approve"]
                    .iter()
                    .any(|verb| method.contains(verb)),
                "{method} looks like a mutation"
            );
        }
    }

    #[test]
    fn publishing_an_identical_fleet_wakes_nobody() {
        let service = FleetService::new(ROOT, fleet());
        assert!(!service.publish(fleet()), "nothing moved");
        let mut changed = fleet();
        changed.rows[1].published = 5;
        assert!(service.publish(changed));
        assert_eq!(service.snapshot().inbox(), 6);
    }

    // --- the wire --------------------------------------------------------

    struct Client {
        stream: BufReader<UnixStream>,
    }

    impl Client {
        async fn connect(socket: &Path) -> Self {
            Self {
                stream: BufReader::new(UnixStream::connect(socket).await.expect("connect")),
            }
        }

        async fn call(&mut self, raw: &str) {
            let mut message = raw.as_bytes().to_vec();
            message.push(0);
            self.stream.get_mut().write_all(&message).await.unwrap();
        }

        /// One framed reply, or `None` if the service hung up.
        async fn reply(&mut self) -> Option<Reply> {
            let mut buf = Vec::new();
            let read = self.stream.read_until(0, &mut buf).await.ok()?;
            if read == 0 {
                return None;
            }
            assert_eq!(buf.pop(), Some(0), "replies are NUL-terminated");
            Some(serde_json::from_slice(&buf).expect("a reply is JSON"))
        }
    }

    /// A socket path nothing else in the suite will pick.
    fn socket_path(name: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("taste-fleetlink-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{name}.sock"))
    }

    async fn started(name: &str) -> (FleetService, std::path::PathBuf) {
        let service = FleetService::new(ROOT, fleet());
        let socket = socket_path(name);
        let listener = FleetService::bind(&socket).expect("bind");
        tokio::spawn(service.clone().serve_on(listener));
        (service, socket)
    }

    /// Two windows on ONE folder derive one socket path, and the second must
    /// not take it.
    ///
    /// This used to unlink unconditionally, on the reasoning that a live
    /// service at this path had to belong to another workspace and therefore
    /// could not exist. The second window's bind then orphaned the first's
    /// listener — the first kept serving a socket no name pointed at any
    /// more, and every shell extension watching the fleet silently followed
    /// the thief. A live service is now refused.
    #[tokio::test]
    async fn a_second_window_on_one_folder_cannot_steal_the_socket() {
        let (service, socket) = started("contended").await;

        let stolen = FleetService::bind(&socket);
        let err = stolen.expect_err("the live socket was taken").to_string();
        assert!(err.contains("another window"), "{err}");

        // ...and the first window is still the one answering on it.
        let mut client = Client::connect(&socket).await;
        client
            .call(r#"{"method":"net.davidstrauss.taste.Fleet.List"}"#)
            .await;
        let body = client.reply().await.unwrap().parameters.unwrap();
        assert_eq!(body["workspaceRoot"], ROOT);
        assert_eq!(body["rows"].as_array().unwrap().len(), 3);
        drop(service);
    }

    /// A window that died leaves its socket file behind. That is not a live
    /// service and must not lock the folder out for good — the next window
    /// clears it and binds.
    #[tokio::test]
    async fn a_dead_windows_socket_is_cleared_rather_than_obeyed() {
        let socket = socket_path("stale");
        // A socket file nobody is listening on: bound, then dropped.
        drop(FleetService::bind(&socket).expect("first bind"));
        assert!(socket.exists(), "the file outlives the listener");

        let listener = FleetService::bind(&socket).expect("stale sockets are leavings");
        tokio::spawn(FleetService::new(ROOT, fleet()).serve_on(listener));

        let mut client = Client::connect(&socket).await;
        client
            .call(r#"{"method":"net.davidstrauss.taste.Fleet.List"}"#)
            .await;
        assert_eq!(
            client.reply().await.unwrap().parameters.unwrap()["workspaceRoot"],
            ROOT
        );
    }

    /// The whole read path, over a real socket: a client that knows only
    /// varlink discovers the interface, reads it, and gets the fleet.
    #[tokio::test]
    async fn a_client_lists_the_fleet_and_can_introspect_the_socket() {
        let (_service, socket) = started("list").await;
        let mut client = Client::connect(&socket).await;

        client
            .call(r#"{"method":"org.varlink.service.GetInfo"}"#)
            .await;
        let reply = client.reply().await.unwrap();
        let info = reply.parameters.unwrap();
        // The product names the folder, so a client holding four sockets
        // can label them without calling into our own interface first.
        assert_eq!(info["product"], format!("Taste IDE — {ROOT}"));
        assert_eq!(info["interfaces"][1], INTERFACE);

        client
            .call(&format!(
                r#"{{"method":"org.varlink.service.GetInterfaceDescription","parameters":{{"interface":"{INTERFACE}"}}}}"#
            ))
            .await;
        let described = client.reply().await.unwrap().parameters.unwrap();
        let description = described["description"].as_str().unwrap();
        assert_eq!(description, DESCRIPTION);
        idl::parse(description).expect("what we serve is what parses");

        // Calls pipeline on one connection: this is the third on the same
        // socket, with no reconnect.
        client
            .call(r#"{"method":"net.davidstrauss.taste.Fleet.List"}"#)
            .await;
        let reply = client.reply().await.unwrap();
        assert!(!reply.continues, "List answers once");
        let body = reply.parameters.unwrap();
        assert_eq!(body["version"], VERSION);
        assert_eq!(body["workspace"], "taste-ide");
        assert_eq!(body["workspaceRoot"], ROOT);
        assert_eq!(body["inbox"], 3);
        assert_eq!(body["spend"]["inputTokens"], 42_000);
        assert_eq!(body["rows"].as_array().unwrap().len(), 3);
        assert_eq!(body["rows"][0]["environment"], "primary");
        assert_eq!(body["rows"][1]["chat"]["busy"], true);
        assert_eq!(body["rows"][2]["chat"], Value::Null);
    }

    /// Watch is the interface's reason to exist: a shell indicator must
    /// learn that a build failed without polling. One event on the IDE
    /// side, one reply on the wire.
    #[tokio::test]
    async fn watch_streams_an_update_when_the_fleet_actually_changes() {
        let (service, socket) = started("watch").await;
        let mut client = Client::connect(&socket).await;
        client
            .call(r#"{"method":"net.davidstrauss.taste.Fleet.Watch","more":true}"#)
            .await;

        // The first reply is the fleet as it stands: no List needed first.
        let first = client.reply().await.unwrap();
        assert!(first.continues, "more replies are coming");
        assert_eq!(first.parameters.unwrap()["rows"][1]["detail"], "running");

        // An environment fails to build — the tagged event the app turns
        // into a publish.
        let mut changed = fleet();
        changed.rows[1].mode = "safe".into();
        changed.rows[1].state = "failed".into();
        changed.rows[1].detail = "no environment · failed: no such image".into();
        assert!(service.publish(changed.clone()));

        let update = client.reply().await.unwrap();
        assert!(update.continues);
        let body = update.parameters.unwrap();
        assert_eq!(body["rows"][1]["state"], "failed");
        assert_eq!(
            body["rows"][1]["detail"],
            "no environment · failed: no such image"
        );

        // A publish that changes nothing sends nothing; the next real
        // change is the next reply, with no stale one in between.
        assert!(!service.publish(changed.clone()), "identical");
        let mut again = changed;
        again.rows[0].shells = 4;
        service.publish(again);
        let update = client.reply().await.unwrap();
        assert_eq!(update.parameters.unwrap()["rows"][0]["shells"], 4);
    }

    #[tokio::test]
    async fn the_standard_errors_are_the_standard_errors() {
        let (_service, socket) = started("errors").await;
        let mut client = Client::connect(&socket).await;

        // Watch without `more` is the mistake a hand-written client makes.
        client
            .call(r#"{"method":"net.davidstrauss.taste.Fleet.Watch"}"#)
            .await;
        assert_eq!(
            client.reply().await.unwrap().error.unwrap(),
            "org.varlink.service.ExpectedMore"
        );

        client
            .call(r#"{"method":"net.davidstrauss.taste.Fleet.Destroy"}"#)
            .await;
        assert_eq!(
            client.reply().await.unwrap().error.unwrap(),
            "org.varlink.service.MethodNotFound"
        );

        client.call(r#"{"method":"com.example.Other.Thing"}"#).await;
        let reply = client.reply().await.unwrap();
        assert_eq!(
            reply.error.unwrap(),
            "org.varlink.service.InterfaceNotFound"
        );
        assert_eq!(reply.parameters.unwrap()["interface"], "com.example.Other");

        client
            .call(r#"{"method":"org.varlink.service.GetInterfaceDescription","parameters":{"interface":"com.example.Other"}}"#)
            .await;
        assert_eq!(
            client.reply().await.unwrap().error.unwrap(),
            "org.varlink.service.InterfaceNotFound"
        );

        // `oneway` means what it says: no reply, and the connection stays
        // usable for the next call.
        client
            .call(r#"{"method":"net.davidstrauss.taste.Fleet.List","oneway":true}"#)
            .await;
        client
            .call(r#"{"method":"net.davidstrauss.taste.Fleet.List"}"#)
            .await;
        let reply = client.reply().await.unwrap();
        assert_eq!(
            reply.parameters.unwrap()["inbox"],
            3,
            "the oneway call produced no reply to confuse this one"
        );

        // Garbage gets one honest answer, then the connection goes.
        client.call("not json").await;
        assert_eq!(
            client.reply().await.unwrap().error.unwrap(),
            "org.varlink.service.InvalidParameter"
        );
        assert!(client.reply().await.is_none(), "and then hangs up");
    }
}
