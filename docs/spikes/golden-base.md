# Spike: a golden base image, and a VM's identity as system credentials

David asked on 2026-10-26 for a VM base the IDE generates once per release
and reuses, with the downloaded source thrown away and fetched again only
when a new base is needed. He also asked that whatever hands each VM its
own identity be able to "seamlessly switch" to systemd's IMDS support once
the guest's systemd has it. This note is the prototype, done by hand on
throwaway VMs against Fedora CoreOS 44.20260928.3.2 (systemd 259.9), with
what it measured and what it turned up. It shipped the same day as
`taste_devcontainer::golden`, with one change of transport (below, "As
shipped").

## What was built

1. **A builder VM**, booted once from the CoreOS base with the Ignition
   `provision::ignition` writes today: the masks, the timers, the sysctls,
   the egress ruleset, and core's podman socket. In it, the baseline image
   was built as `core`, exactly as a VM builds it now. That took 44 s.
2. **Generalised**, as root: the ssh host keys, core's authorized keys, and
   `/etc/hostname` removed; `/etc/machine-id` set to `uninitialized`; the
   random seed, `credential.secret`, NetworkManager's state, the journal,
   and the audit log removed; synced, waited 30 s, trimmed (see
   `provision::GUEST_TIDY_SERVICE` for why it waits), and powered off.
3. **Flattened** by `qemu-img convert -c -O qcow2 -o compression_type=zstd`
   into a standalone base with no backing file. That took 10 s.
4. **Clones** were overlays on that base, booted with **no Ignition at
   all**. Each one's identity came in as systemd system credentials, in
   SMBIOS type 11 strings (libvirt's `<sysinfo type='smbios'><oemStrings>`
   and `<smbios mode='sysinfo'/>`).

## What it measured

| | Today | Golden base |
| --- | --- | --- |
| base on disk | 1.84 GiB (CoreOS only) | 1.18 GiB, baseline image included |
| first boot, kernel to `multi-user` | 32 s (Ignition, grow root) | 7.0 s uncontended |
| ssh answering after `virsh start` | — | 10 s uncontended |
| baseline image in a new VM | built (44 s) or loaded from the kept copy | already there, and runs |

Zstd on CoreOS alone gives 1.03 GiB, so the baseline image costs the base
about 150 MiB compressed.

## A VM's identity, as credentials

The interface is the **credential**, not the transport. Every clone is
given the same two:

- `system.hostname=<domain name>`. PID1 sets the kernel hostname from it at
  the earliest point ("Initializing hostname from credential").
- `tmpfiles.extra` (as `io.systemd.credential.binary:`, base64), which
  `systemd-tmpfiles-setup` applies early in boot:

  ```
  f+ /etc/hostname 0644 root root - <domain name>
  d /var/home/core/.ssh 0700 core core -
  d /var/home/core/.ssh/authorized_keys.d 0700 core core -
  f+~ /var/home/core/.ssh/authorized_keys.d/taste 0600 core core - <base64 public key>
  f+~ /etc/ssh/ssh_host_ed25519_key 0600 root root - <base64 private key>
  f+~ /etc/ssh/ssh_host_ed25519_key.pub 0644 root root - <base64 public key>
  ```

Checked on clones: ssh with `StrictHostKeyChecking=yes` against the minted
host key; core's key and no other; the static hostname held for over a
minute past DHCP; a machine-id of its own, taken from the domain's SMBIOS
UUID; and no failed units. The same credential got the builder back in
after it had been generalised, which is also how a base gets touched up.

**Why this switches to IMDS without a guest change.** systemd's IMDS
support (`systemd-imdsd@.service`, `systemd-imds-import.service`, the
generator keyed on `hwdb.d/40-imds.hwdb`; parts of it as late as systemd
261) imports a cloud instance's userdata `systemd.credentials` array into
`/run/credstore/`, and consumers take their credentials from there with
`ImportCredential=` as they do from SMBIOS. So the cloud path would put
the same two credentials in the instance's userdata, and the guest would
not know the difference. The one thing to check at switch time is
ordering: that the import lands before `systemd-tmpfiles-setup`, which
on 259 runs before the network is up.

## What it turned up

- **systemd 259's firstboot does not read a hostname credential.**
  `systemd-firstboot` reads `firstboot.locale`, `firstboot.locale-messages`,
  `firstboot.keymap`, and `firstboot.timezone` and nothing else. The
  `firstboot.hostname` the IMDS import produces comes with the IMDS work.
  CoreOS also ships `systemd-firstboot.service` with nothing pulling it in.
  So the hostname rides `system.hostname` and `/etc/hostname` in
  `tmpfiles.extra`, and firstboot is not needed at all.
- **A first boot's presets remove a `.wants` symlink to a static unit.**
  `/etc/systemd/system/sysinit.target.wants/systemd-firstboot.service`,
  made in the builder, was removed on the clone's first boot ("Removed
  …"). A drop-in on the target (`sysinit.target.d/…`, `Wants=`) survived.
  Anything the base pulls in must be pulled in that way, or enabled
  through `[Install]` and a preset. The timers Ignition enabled survived.
- **A transient hostname loses to DHCP, and DHCP carries the host's own
  name.** passt hands the guest the host's hostname, and NetworkManager
  takes it when no static hostname is set: a clone with only
  `system.hostname` was called `t16gen1`, the host's name, a minute in.
  Hence the static `/etc/hostname` in `tmpfiles.extra`. Today's VMs are
  safe because Ignition writes `/etc/hostname`.
- **The initrd runs `tmpfiles.extra` too**, and fails on `core`, who does
  not exist there ("Failed to resolve user 'core'"). It is harmless: the
  real root's run, a moment later, writes everything. It is noise in the
  journal, and a reason not to switch to numeric ids, which would write
  the host key into the initrd's tmpfs.
- **Ignition does not run again.** Without `ignition.firstboot` on the
  command line (CoreOS removes the flag after the builder's first boot),
  a clone boots past Ignition. Re-arming it is not a supported interface,
  and this design does not need it.

## As shipped: fw_cfg files, not SMBIOS strings

The prototype handed the credentials over as SMBIOS type 11 strings. libvirt
puts those on qemu's command line, and every user of the host can read a
process's command line in `/proc`: the guest's private host key would have
been there. systemd reads the same credentials from `fw_cfg` as
`opt/io.systemd.credentials/<name>`, and libvirt's `<sysinfo type='fwcfg'>`
takes each as a **file**, which is how the Ignition config already
travelled. Checked on systemd 259 before building on it: both credentials
arrived ("Acquired 2 regular credentials"), and qemu's command line named
only the files. A credential's name has 28 bytes there (`fw_cfg` names are
55), which both of these fit.

## What wiring it in would take

- `guest`: after a release's base is verified, build its golden base,
  following 1–3 above. The builder is a VM of no workspace, which no agent
  ever touches, so its baseline image may be shared across projects. That
  is what `image::ensure_image_kept` cannot offer, since its copy comes
  out of a VM where agents ran. Then remove the CoreOS file. The base is
  named for its release and a hash of what went into it (the Ignition's
  generic part and the baseline config), so a change to either builds a
  new one, and `prune_unused_bases` collects the old ones as it does now.
- `provision`: a local VM is an overlay on the golden base, its
  per-VM Ignition replaced by the two credentials in `<sysinfo
  type='smbios'>`. The Ignition's generic part moves to the builder
  unchanged. The cloud path keeps CoreOS and Ignition until the guest's
  systemd has IMDS; then it sends the same credentials in userdata, built
  from the same function.
- The daily package refresh still rebuilds the baseline image in each VM,
  so the copy baked into the base matters for a new VM's first start, and
  the base is rebuilt with each release rather than daily.

## Sources

- [System and Service Credentials](https://systemd.io/CREDENTIALS/), for
  SMBIOS type 11, fw_cfg, the well-known credentials, and the IMDS import
- [systemd-imdsd@.service(8)](https://www.man7.org/linux/man-pages/man8/systemd-imdsd@.service.8.html)
- [systemd-imds-generator(8)](https://man7.org/linux/man-pages/man8/systemd-imds-generator.8.html)
