//! Kernel confinement for the IDE's helper processes.
//!
//! The IDE's own process is the host side of the boundary: the Flatpak
//! grants it the home directory, the network, and `org.freedesktop.Flatpak`
//! (which runs commands on the host), so any code linked into it acts with
//! all of that. Work that parses text a repository or an agent wrote, with
//! code the IDE did not write, is done in a HELPER instead, and the helper
//! is confined here before it executes its first instruction (David,
//! 2026-10-09: "Build the sandboxed helper first, then add Mermaid"). A
//! dependency that turns hostile, or a parser bug an input can drive, then
//! owns a process that can read some system files and talk to its parent
//! over the pipes it was handed — nothing else.
//!
//! What a confined process gets, all of it applied in the child between
//! `fork` and `exec`, so it holds from the program's first instruction:
//!
//! - **Landlock, filesystem**: read (and execute) beneath the paths the
//!   [`Policy`] names, and nothing else — no write, create, remove,
//!   rename, truncate, or device ioctl anywhere. A path that does not
//!   exist is skipped; one that cannot be opened is an error.
//! - **Landlock, network**: no TCP bind or connect (ABI 4 on).
//! - **Landlock, scoping**: no abstract unix sockets and no signals to
//!   processes outside the sandbox (ABI 6 on).
//! - **seccomp**: no `socket` or `socketpair` at all — which is what closes
//!   the session bus, a *pathname* socket Landlock does not govern, and
//!   with it the portal that runs commands on the host — and no io_uring
//!   (which can open sockets around seccomp), ptrace, cross-process memory
//!   access, BPF, perf, userfaultfd, keyrings, namespaces, or mounts. A
//!   foreign syscall ABI is killed, not checked.
//! - **No inherited descriptors** past stdio: every other one is marked
//!   close-on-exec, so a socket the parent holds without the flag (a
//!   library's D-Bus connection, say) does not cross.
//! - **`no_new_privs`**, no core dumps, and an address-space ceiling.
//!
//! **Not best-effort.** A kernel whose Landlock is missing, disabled, or
//! older than ABI 3 (truncate is the first right a write-free policy needs
//! that ABI 1 cannot deny) makes [`confine`] fail, and the caller does
//! without the helper. The same rule the agent sandbox keeps.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// What a confined process may read.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// Files it may execute (and read): its own program and the ELF
    /// interpreter the kernel loads it with. Files only — a directory here
    /// would make every program beneath it runnable, `/bin/sh` included.
    pub exec: Vec<PathBuf>,
    /// Read and list beneath each, and nothing more.
    pub read: Vec<PathBuf>,
    /// The ceiling on the process's address space, in bytes.
    pub memory: u64,
}

impl Policy {
    /// The system as a helper needs it to start: its own program and its
    /// interpreter to execute; the directories its libraries load from,
    /// and the dynamic linker's cache, to read. Inside the Flatpak the
    /// app's own tree is `/app`.
    pub fn for_program(program: &Path) -> Result<Self> {
        let mut exec = vec![program.to_path_buf()];
        if let Some(interpreter) = interpreter(program)? {
            exec.push(interpreter);
        }
        let read = ["/usr", "/lib", "/lib64", "/app", "/etc/ld.so.cache"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        Ok(Self {
            exec,
            read,
            memory: 4 << 30,
        })
    }
}

impl Policy {
    /// The fonts a process that sets type needs to read: the system's
    /// (beneath `/usr` already), fontconfig's configuration, the host's
    /// as the Flatpak exposes them, and the user's own font directories —
    /// those, and nothing else in the home.
    pub fn with_fonts(mut self) -> Self {
        self.read.extend(
            [
                "/etc/fonts",
                "/run/host/fonts",
                "/run/host/user-fonts",
                "/run/host/local-fonts",
            ]
            .into_iter()
            .map(PathBuf::from),
        );
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            self.read.push(home.join(".fonts"));
            self.read.push(
                std::env::var_os("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".local/share"))
                    .join("fonts"),
            );
        }
        self
    }
}

/// The ELF interpreter `program` names (`PT_INTERP`), if it is dynamically
/// linked: the one other file the kernel opens to execute it.
fn interpreter(program: &Path) -> Result<Option<PathBuf>> {
    let elf = std::fs::read(program).with_context(|| format!("reading {}", program.display()))?;
    let u16_at = |at: usize| {
        elf.get(at..at + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    let u32_at = |at: usize| {
        elf.get(at..at + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    let u64_at = |at: usize| {
        elf.get(at..at + 8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    };
    // A 64-bit little-endian ELF: the only kind the architectures with a
    // seccomp filter below produce.
    if elf.get(0..6) != Some(&[0x7f, b'E', b'L', b'F', 2, 1]) {
        bail!(
            "{} is not a 64-bit little-endian ELF program",
            program.display()
        );
    }
    let (Some(phoff), Some(phentsize), Some(phnum)) = (u64_at(0x20), u16_at(0x36), u16_at(0x38))
    else {
        bail!("{} has a truncated ELF header", program.display());
    };
    for index in 0..usize::from(phnum) {
        let header = phoff as usize + index * usize::from(phentsize);
        if u32_at(header) != Some(PT_INTERP) {
            continue;
        }
        let (Some(offset), Some(size)) = (u64_at(header + 8), u64_at(header + 32)) else {
            bail!("{} has a truncated program header", program.display());
        };
        let path = elf
            .get(offset as usize..(offset + size) as usize)
            .context("the interpreter path lies outside the file")?;
        let path = path.split(|byte| *byte == 0).next().unwrap_or_default();
        return Ok(Some(PathBuf::from(std::ffi::OsStr::from_bytes(path))));
    }
    Ok(None)
}

const PT_INTERP: u32 = 3;

/// The Landlock ABI this kernel speaks, or `None` when it has none (not
/// built, not enabled at boot).
pub fn landlock_abi() -> Option<u32> {
    // SAFETY: the documented version query — a null attribute, size zero,
    // and the version flag — reads and writes no memory.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    (abi > 0).then_some(abi as u32)
}

/// Confine `command` by `policy`: everything is prepared here, in the
/// parent, and the child only makes the syscalls that apply it.
pub fn confine(command: &mut Command, policy: &Policy) -> Result<()> {
    let abi = landlock_abi().context(
        "this kernel has no Landlock (built without it, or not enabled at boot), so a \
         helper cannot be confined, and helpers only run confined",
    )?;
    if abi < 3 {
        bail!(
            "this kernel's Landlock is ABI {abi}; confining a helper needs ABI 3 (Linux 6.2) \
             or newer, and helpers only run confined"
        );
    }
    let ruleset = ruleset(abi, policy)?;
    let filter = seccomp_filter()?;
    let memory = policy.memory;
    let ruleset_fd = ruleset.as_raw_fd();
    // SAFETY: the closure runs in the forked child, before exec, where only
    // async-signal-safe work is allowed. It allocates nothing and takes no
    // locks: every value it reads was built above, and every call is a
    // bare syscall. The ruleset is moved in so its descriptor lives until
    // exec, which closes it (Landlock makes it close-on-exec).
    unsafe {
        command.pre_exec(move || {
            let _keep = &ruleset;
            apply(ruleset_fd, &filter, memory)
        });
    }
    Ok(())
}

/// The child's half: the order matters. `no_new_privs` first, because
/// both Landlock and an unprivileged seccomp filter require it; Landlock
/// before seccomp, because the filter does not allow the Landlock calls
/// to be made twice but would allow them once; descriptors last, so
/// nothing the steps before opened survives.
fn apply(ruleset_fd: i32, filter: &Filter, memory: u64) -> io::Result<()> {
    // SAFETY: each call is a syscall with arguments built in the parent;
    // none allocates.
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::syscall(libc::SYS_landlock_restrict_self, ruleset_fd, 0u32) != 0 {
            return Err(io::Error::last_os_error());
        }
        let program = libc::sock_fprog {
            len: filter.0.len() as u16,
            filter: filter.0.as_ptr() as *mut libc::sock_filter,
        };
        if libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            0u32,
            &program as *const libc::sock_fprog,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
        let limit = libc::rlimit {
            rlim_cur: memory,
            rlim_max: memory,
        };
        if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
            return Err(io::Error::last_os_error());
        }
        let none = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::setrlimit(libc::RLIMIT_CORE, &none) != 0 {
            return Err(io::Error::last_os_error());
        }
        // Close-on-exec rather than closed: the standard library reports a
        // failed exec through a pipe of its own above stdio, already
        // close-on-exec, and closing it here would turn that report into
        // silence.
        if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, CLOSE_RANGE_CLOEXEC) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

// --- Landlock, from <linux/landlock.h> ---------------------------------
//
// libc carries the syscall numbers and nothing else, so the ABI is written
// out here, field for field.

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1 << 0;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

const ACCESS_FS_EXECUTE: u64 = 1 << 0;
const ACCESS_FS_READ_FILE: u64 = 1 << 2;
const ACCESS_FS_READ_DIR: u64 = 1 << 3;
/// Every filesystem right ABI 1 knows: execute, write, read, list,
/// remove, and make each kind of node.
const ACCESS_FS_V1: u64 = (1 << 13) - 1;
const ACCESS_FS_REFER: u64 = 1 << 13;
const ACCESS_FS_TRUNCATE: u64 = 1 << 14;
const ACCESS_FS_IOCTL_DEV: u64 = 1 << 15;
const ACCESS_NET_BIND_TCP: u64 = 1 << 0;
const ACCESS_NET_CONNECT_TCP: u64 = 1 << 1;
const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;

const CLOSE_RANGE_CLOEXEC: u32 = 1 << 2;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// Every right this ABI can deny: the ruleset handles all of them, so
/// whatever no rule grants is refused.
fn handled(abi: u32) -> RulesetAttr {
    let mut fs = ACCESS_FS_V1;
    if abi >= 2 {
        fs |= ACCESS_FS_REFER;
    }
    if abi >= 3 {
        fs |= ACCESS_FS_TRUNCATE;
    }
    if abi >= 5 {
        fs |= ACCESS_FS_IOCTL_DEV;
    }
    RulesetAttr {
        handled_access_fs: fs,
        handled_access_net: if abi >= 4 {
            ACCESS_NET_BIND_TCP | ACCESS_NET_CONNECT_TCP
        } else {
            0
        },
        scoped: if abi >= 6 {
            SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL
        } else {
            0
        },
    }
}

fn ruleset(abi: u32, policy: &Policy) -> Result<OwnedFd> {
    let attr = handled(abi);
    // Below ABI 6 the kernel's attribute has no `scoped`; it accepts the
    // longer struct as long as the extra field is zero, which `handled`
    // makes it.
    // SAFETY: a valid attribute and its size.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const RulesetAttr,
            std::mem::size_of::<RulesetAttr>(),
            0u32,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("creating the Landlock ruleset");
    }
    // SAFETY: the kernel just returned this descriptor, and nothing else
    // owns it.
    let ruleset = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    let exec = ACCESS_FS_EXECUTE | ACCESS_FS_READ_FILE;
    let read = ACCESS_FS_READ_FILE | ACCESS_FS_READ_DIR;
    for (paths, access) in [(&policy.exec, exec), (&policy.read, read)] {
        for path in paths {
            allow(&ruleset, path, access)?;
        }
    }
    Ok(ruleset)
}

/// Grant `access` beneath `path`. A file takes only the rights a file
/// has; a path that is not there is skipped, since a rule for it would
/// grant nothing.
fn allow(ruleset: &OwnedFd, path: &Path, access: u64) -> Result<()> {
    let Ok(meta) = std::fs::metadata(path) else {
        return Ok(());
    };
    let access = if meta.is_dir() {
        if access & ACCESS_FS_EXECUTE != 0 {
            bail!(
                "{} is a directory, and execute is granted to files only",
                path.display()
            );
        }
        access
    } else {
        access & !ACCESS_FS_READ_DIR
    };
    let name = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("{} has a NUL in it", path.display()))?;
    // SAFETY: a NUL-terminated path; O_PATH opens without reading.
    let fd = unsafe { libc::open(name.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("opening {}", path.display()));
    }
    // SAFETY: as above, ours alone.
    let parent = unsafe { OwnedFd::from_raw_fd(fd) };
    let rule = PathBeneathAttr {
        allowed_access: access,
        parent_fd: parent.as_raw_fd(),
    };
    // SAFETY: a valid rule for a valid ruleset.
    let added = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.as_raw_fd(),
            LANDLOCK_RULE_PATH_BENEATH,
            &rule as *const PathBeneathAttr,
            0u32,
        )
    };
    if added != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("allowing {}", path.display()));
    }
    Ok(())
}

// --- seccomp -----------------------------------------------------------

/// A built BPF program, kept whole until the child installs it.
struct Filter(Vec<libc::sock_filter>);

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

/// x86_64's x32 ABI shares the architecture tag and marks its syscalls
/// with this bit: a filter that checked the number alone would let an x32
/// `socket` through.
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

/// What a confined helper may never call.
fn denied() -> Vec<libc::c_long> {
    vec![
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_mount,
        libc::SYS_open_by_handle_at,
        libc::SYS_name_to_handle_at,
    ]
}

fn seccomp_filter() -> Result<Filter> {
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    bail!("no seccomp filter is written for this architecture, so helpers cannot run here");

    let statement = |code: u32, k: u32| libc::sock_filter {
        code: code as u16,
        jt: 0,
        jf: 0,
        k,
    };
    let jump = |k: u32, jt: u8, jf: u8| libc::sock_filter {
        code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
        jt,
        jf,
        k,
    };
    let load = |offset: u32| statement(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, offset);
    let ret = |value: u32| statement(libc::BPF_RET | libc::BPF_K, value);
    // seccomp_data: nr at 0, arch at 4. Jump offsets count the
    // instructions skipped after the jump itself.
    let mut program = vec![
        load(4),
        // The native ABI, or the process dies.
        jump(AUDIT_ARCH, 1, 0),
        ret(libc::SECCOMP_RET_KILL_PROCESS),
        load(0),
    ];
    #[cfg(target_arch = "x86_64")]
    program.extend([
        // nr >= the x32 bit: fall through to the kill; below it, skip it.
        libc::sock_filter {
            code: (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16,
            jt: 0,
            jf: 1,
            k: X32_SYSCALL_BIT,
        },
        ret(libc::SECCOMP_RET_KILL_PROCESS),
    ]);
    let denied = denied();
    for (index, nr) in denied.iter().enumerate() {
        // A match skips the comparisons after this one and the allow, and
        // lands on the refusal.
        let after = (denied.len() - index - 1) as u8;
        program.push(jump(*nr as u32, after + 1, 0));
    }
    program.push(ret(libc::SECCOMP_RET_ALLOW));
    program.push(ret(libc::SECCOMP_RET_ERRNO | (libc::EPERM as u32)));
    if program.len() > u16::MAX as usize {
        bail!("the seccomp filter is too long");
    }
    Ok(Filter(program))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// The probe the tests run confined: this very test binary, asked to
    /// run one ignored test that tries each forbidden thing and prints
    /// what happened. Running itself is how a test gets a child whose
    /// every instruction is ours.
    fn probe(target: &Path) -> String {
        let exe = std::env::current_exe().unwrap();
        let policy = Policy::for_program(&exe).unwrap();
        let mut command = Command::new(&exe);
        command
            .args([
                "--ignored",
                "--exact",
                "tests::confined_probe",
                "--nocapture",
            ])
            .env("TASTE_CONFINE_PROBE", target)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        confine(&mut command, &policy).expect("this kernel confines");
        let output = command.output().expect("the probe runs");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Run only as the confined child of [`a_confined_process_reaches_nothing_it_was_not_given`].
    #[test]
    #[ignore]
    fn confined_probe() {
        let Some(target) = std::env::var_os("TASTE_CONFINE_PROBE") else {
            return;
        };
        let target = PathBuf::from(target);
        let say = |what: &str, ok: bool| println!("PROBE {what}={}", if ok { "yes" } else { "no" });
        say("read", std::fs::read(&target).is_ok());
        say(
            "write",
            std::fs::write(target.with_extension("new"), b"x").is_ok(),
        );
        say(
            "truncate",
            std::fs::OpenOptions::new()
                .write(true)
                .open(&target)
                .is_ok(),
        );
        say("list", std::fs::read_dir(target.parent().unwrap()).is_ok());
        say(
            "unix",
            std::os::unix::net::UnixStream::connect("/run/dbus/system_bus_socket").is_ok()
                || std::os::unix::net::UnixDatagram::unbound().is_ok(),
        );
        say("tcp", std::net::TcpListener::bind("127.0.0.1:0").is_ok());
        say(
            "exec",
            Command::new("/bin/sh")
                .arg("-c")
                .arg("true")
                .status()
                .is_ok(),
        );
        say(
            "system",
            std::fs::read("/usr/lib/os-release").is_ok()
                || std::fs::read("/etc/ld.so.cache").is_ok(),
        );
    }

    #[test]
    fn a_confined_process_reaches_nothing_it_was_not_given() {
        if landlock_abi().is_none_or(|abi| abi < 3) {
            eprintln!("no Landlock ABI 3 here: confinement is refused, as tested below");
            return;
        }
        let dir = std::env::temp_dir().join(format!("taste-confine-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let secret = dir.join("secret");
        std::fs::write(&secret, b"the user's").unwrap();
        // The same file is readable unconfined: what the probe reports is
        // the confinement, not the fixture.
        assert!(std::fs::read(&secret).is_ok());

        let report = probe(&secret);
        for (what, expected) in [
            ("read", "no"),
            ("write", "no"),
            ("truncate", "no"),
            ("list", "no"),
            ("unix", "no"),
            ("tcp", "no"),
            ("exec", "no"),
            ("system", "yes"),
        ] {
            assert!(
                report.contains(&format!("PROBE {what}={expected}")),
                "{what} should be {expected}:\n{report}"
            );
        }
        assert!(!dir.join("secret.new").exists());
        assert_eq!(std::fs::read(&secret).unwrap(), b"the user's");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file rule takes only file rights: `READ_DIR` on a file is an
    /// error the kernel would return, and the program's own path is one.
    #[test]
    fn a_program_path_is_granted_as_a_file() {
        if landlock_abi().is_none_or(|abi| abi < 3) {
            return;
        }
        let exe = std::env::current_exe().unwrap();
        let mut command = Command::new(&exe);
        let policy = Policy::for_program(&exe).unwrap();
        assert!(
            policy.exec.len() == 2,
            "a test binary is dynamically linked: {policy:?}"
        );
        confine(&mut command, &policy).unwrap();
    }

    #[test]
    fn every_right_the_abi_has_is_handled() {
        assert_eq!(handled(3).handled_access_fs, (1 << 15) - 1);
        assert_eq!(handled(3).handled_access_net, 0);
        assert_eq!(handled(4).handled_access_net, 0b11);
        assert_eq!(handled(5).handled_access_fs, (1 << 16) - 1);
        assert_eq!(handled(6).scoped, 0b11);
    }
}
