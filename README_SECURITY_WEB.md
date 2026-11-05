# Security changes in SalamanderRS Web v0.9

Confined writable mode accepts arbitrary canonical DATA/MODELS directories;
the old `/tmp` limitation has been removed. The Linux Landlock ABI v3
sandbox remains mandatory in this mode and is installed before Tokio worker
threads are created. Startup fails closed if it cannot be applied. Either
confined root equal to `/` is now rejected; unrestricted mode is the only way
to configure the host root. Confined writable mode also rejects UID 0 to
avoid accidentally running a unauthenticated web file manager as root.

`--verify-sandbox` no longer assumes that `/tmp` and `/etc/passwd` are always
outside the configured roots. The read and write probes are selected and
checked before Landlock is applied; then their access is expected to fail.
Both configured roots are tested for the ability to create/remove a file.
No production filesystem is automatically mounted or contacted by this test.

**This is not a production-safe change.** Confined mode STILL has no HTTP
authentication; loopback alone does not isolate local processes. Race-prone
F2/F5/F6/F7/F8, ZIP and directory listing, project quotas, user identity,
connector routing, seccomp and mount namespace isolation all remain to be
addressed before using `/AI/data/<username>` and `/AI/models/<username>`.
The `--unrestricted` administrator mode remains especially dangerous and
must not be exposed over the network.

---

# Additional security notes: SalamanderRS Web v0.8

**Default behavior has NOT been loosened:** read-only unless the operator
explicitly enables the existing `/tmp` writable test mode with Landlock.

`--unrestricted` is an intentionally dangerous, explicit *local administrator*
mode. It bypasses the `/tmp` writable-root guard and the Landlock filesystem
sandbox. This is useful for administrative experiments, **not** a security
recommendation for root deployments. The HTTP server still binds strictly to
127.0.0.1 and requires HTTP Basic auth for every route in unrestricted mode.
The password comes from `SALAMANDER_ADMIN_PASSWORD` (min. 16 characters), the
username is `admin`. Do not expose it through a reverse proxy, LAN, Tailscale,
or public HTTP. Use an SSH tunnel and restrict local access to the server.
Basic auth alone does not make a privileged web file manager trustworthy.

The new F4 editor opens via descriptor-relative `openat2` without following
symlinks. Saving rejects non-UTF8, oversized files, hardlinks and existing
xattrs/ACL, as well as inherited xattrs/ACL on the temporary inode. It writes
into a local temporary file and atomically replaces the destination, attempting
to preserve owner and POSIX mode. Another process can still race between the
last validation and rename. Back up critical files before use; **do not use
this build as a production root-level configuration editor**.

The earlier risks concerning TOCTOU, ZIP and core path-based operations,
per-user authorization, cgroups, and quota handling remain unresolved. The
/default/ mode is still a test prototype without HTTP authentication: loopback
only, never a Tailscale-served production endpoint.

---

# Security review: SalamanderRS Web v0.6 -> v0.7 (test mode)

## What changed in v0.7

- **Linux Landlock confinement** on the writable code path. A fail-closed
  sandbox is installed on the main thread before the Tokio thread pool is
  constructed. The ruleset allows file reads and ordinary writes only within
  the two canonical roots (DATA and MODELS). It denies execution and special
  file creation. Requires kernel Landlock ABI v3 or higher. The existing
  `/tmp` writable-roots check is still mandatory.
- **ZIP validation** now streams entries and rejects absolute or parent paths,
  symlinks, hardlinks, encryption, unsupported kinds and excessive depth.
  It no longer creates a potentially unbounded cached member index for an
  untrusted upload. The extraction worker independently enforces limits on
  **actual** decompressed bytes (20 GiB) and entry count (20,000), regardless
  of the archive's claimed size or replacement after preflight.
- Added an isolated `--verify-sandbox` mode. It must be run as a separate
  process and confirms reads and writes outside the configured roots fail
  and a test file inside DATA can be created and removed.
- Tests cover the ZIP extraction budget and path normalization.
- Removed an unused import warning in the upload/download module.

## High-priority unresolved risks (do NOT use on production data)

1. **No HTTP user authentication.** The `X-Salamander-Request` header is not
   authentication. Any process able to contact the loopback port can access
   the test roots. Do not expose the port via LAN, Tailscale or a reverse proxy.
2. **Pathname-based operations still have TOCTOU races.** F2/F5/F6/F7/F8,
   directory listing and ZIP use pathname traversal. Landlock is a defense
   in depth against file read/write escapes, but does not constrain every
   metadata operation (`chmod`, `chown`, `utime`, `setxattr`) on current Linux.
   Production requires descriptor-relative operations and container-level
   filesystem isolation; neither is implemented by this patch.
3. **Same-UID cross-process interference.** Other processes with write access
   to the same directories can rename entries or replace symlinks while
   SalamanderRS runs. Only in-process write serialization exists.
4. **Resource exhaustion.** ZIP streaming has byte/entry limits but the
   service is not isolated with cgroup CPU/RAM/IO limits. Upload uses a per-file
   cap but no aggregate rate or session quota. A failed extraction may leave
   successfully completed earlier files behind.
5. **Quotas and encryption.** UI still displays filesystem availability only;
   it has no authoritative XFS project-quota data. Encrypted/mounted trees need
   verified access rules from AI Platform and safe propagation of mounts.
6. **Future SFTP.** The current sandbox denies executable launches and reads
   outside roots, so adding OpenSSH client integration will require explicit
   rules and a separate threat review; SFTP is not enabled in v0.7.

## Build and verification on ailab

Unzip into a NEW directory, without overwriting the working v0.6 build.
Run `cargo test -p salamander-core -p salamander-web`, then
`cargo build --release -p salamander-web` from the workspace root.

```bash
SALAMANDER_DATA_ROOT=/tmp/salamander-test/data \
SALAMANDER_MODELS_ROOT=/tmp/salamander-test/models \
SALAMANDER_ENABLE_WRITES=1 \
./target/release/salamander-rs-web --verify-sandbox
```

Expected: `Landlock verification succeeded: outside reads and writes denied.`
If Landlock reports `ENOSYS`, `EPERM` or insufficient ABI, do not bypass it;
collect the exact error and kernel information (`uname -r`,
`cat /sys/kernel/security/lsm`) and verify host/container policy.

This source was not compiled in the author's assistant environment; compilation
and runtime checks must be performed on `ailab`. Keep v0.6 as a fallback.
