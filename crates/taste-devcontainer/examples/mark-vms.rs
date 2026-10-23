//! Judge, by hand, the VMs on this host that nothing has marked
//! (`taste_devcontainer::disposable`): each shut-off one is booted in turn,
//! looked at with its containers stopped, marked when nothing on it is
//! missing from its folder here, and shut down again — the look a window's
//! close takes, for VMs whose window closed before closes took it, or never
//! got to. A marked VM is then removed by the next launch once it has gone
//! unused past `IDLE_RETIREMENT`, or sooner by one short of room.
//!
//! ```text
//! mark-vms [DOMAIN…]
//! ```
//!
//! With no domains, every unmarked shut-off VM. A VM whose folder an IDE
//! has open is skipped: that IDE may bring it up meanwhile. One VM runs at
//! a time, since each commits its whole memory while it does.
//!
//! Build it in the devcontainer and run it on the host, where libvirt is:
//! `cargo build -p taste-devcontainer --example mark-vms`, then
//! `target/debug/examples/mark-vms`.

use std::time::Duration;

use anyhow::Result;
use taste_devcontainer::disposable::{self, Verdict};
use taste_devcontainer::provision::{DomainState, LibvirtSession};

#[tokio::main]
async fn main() -> Result<()> {
    let only: Vec<String> = std::env::args().skip(1).collect();
    let libvirt = LibvirtSession::new();
    for vm in libvirt.list_all().await? {
        if !only.is_empty() && !only.contains(&vm.domain) {
            continue;
        }
        let folder = vm.workspace_root.display();
        if vm.state != DomainState::ShutOff || disposable::is_marked(&vm) {
            println!("{}: skipped, running or already marked", vm.domain);
            continue;
        }
        if taste_core::instance::held_elsewhere(&vm.workspace_root) {
            println!("{}: skipped, {folder} is open in an IDE", vm.domain);
            continue;
        }
        println!("{} ({folder}): booting", vm.domain);
        libvirt.start(&vm).await?;
        let verdict = match libvirt.wait_ready(&vm, Duration::from_secs(300)).await {
            Ok(()) => disposable::look_and_mark(&vm, true, false).await,
            Err(e) => Err(e),
        };
        match verdict {
            Ok(Verdict::Marked { checkouts }) => println!(
                "{}: marked, {checkouts} checkout(s), all on this machine",
                vm.domain
            ),
            Ok(Verdict::Kept { reasons }) => {
                println!("{}: kept", vm.domain);
                for reason in reasons {
                    println!("  {reason}");
                }
            }
            Err(e) => println!("{}: not judged: {e:#}", vm.domain),
        }
        libvirt.power_off(&vm, Duration::from_secs(120)).await?;
        println!("{}: shut down", vm.domain);
    }
    Ok(())
}
