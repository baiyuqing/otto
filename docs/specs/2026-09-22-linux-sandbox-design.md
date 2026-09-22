# Linux sandbox driver design (Landlock + seccomp)

Status: proposed 2026-09-22. Supersedes the design note in
[issue #174](https://github.com/baiyuqing/otto/issues/174), which recorded the
survey this spec resolves. **Not approved and not implemented**: no production
code should be written against it until the three decisions under
"Decisions to confirm" are settled.

## Goal

Give Linux a real confined driver, so `bash` is registered there under the
same policy contract macOS gets from Seatbelt, instead of requiring
`--sandbox off`.

Since [#173](https://github.com/baiyuqing/otto/pull/173) Otto builds and runs
on Linux with no confined driver: `auto` and `seatbelt` report
`unsupported-platform`, `open_sandbox_runtime` fails closed, and the `bash`
tool is not registered at all.

Non-goals for this iteration: container or VM isolation, a Landlock policy
for anything other than the `bash` tool's children, per-command policy
overrides, and any mechanism that needs root, a setuid helper, an external
binary, or an unprivileged user namespace.

## What the existing contract already gives

The driver abstraction does not need reshaping. Three things already hold:

- **`supports_policy` fails closed for free.** That function in
  `crates/otto/src/sandbox/mod.rs` refuses `FilesystemMode::WorkspaceWrite`
  unless the driver advertises
  `read_confinement`, `write_confinement` **and** `unix_socket_deny`, then
  matches `network_deny`/`network_allow` against the requested
  `NetworkMode`. A driver that honestly reports a capability it cannot
  enforce on this kernel is refused by `Executor::new` with
  `Error::UnsupportedPolicy`. No new gating code, and no path by which a
  weaker sandbox than the policy asked for ships silently.
- **`PrivateDirectories { root, home, temp, cache }`** already exists, created
  at 0700, and `sandbox::environment` already builds the child's entire
  environment. The child's `HOME`/`TMPDIR`/cache are Otto's to set.
- **`sandbox::conformance` is driver-agnostic.** `Contract` plus
  `driver_contract!` already run the same checks against Seatbelt and the
  direct driver, including process-group teardown, environment replacement,
  and the `advertised_*` capability checks. A Landlock driver inherits all of
  them by implementing one trait.

`DriverId` is a validated `[a-z0-9-]` string, so `"landlock"` needs no type
change.

## Mechanisms

**Landlock** is a kernel LSM: a process applies an unprivileged, irrevocable
path allowlist **to itself**. No namespaces, no helper binary, and it composes
inside a container, which matters because Otto is frequently run in Docker.

| ABI | Kernel | Adds |
| --- | --- | --- |
| 1 | 5.13 | filesystem read/write/execute |
| 2 | 5.19 | cross-directory rename/link |
| 3 | 6.2 | truncate |
| 4 | **6.7** | **TCP bind/connect** |
| 5 | 6.10 | device ioctl |
| 6 | 6.12 | abstract Unix sockets, signal scoping |
| 7 | 6.15 | audit logging control |
| 8–11 | newer | TSYNC, pathname Unix sockets, UDP, `no_new_privs` flag |

**seccomp-bpf** filters syscalls by number and by scalar arguments. It cannot
express path policy, but it covers exactly what Landlock cannot on the kernels
people actually run: `socket(AF_UNIX)`, `socket(AF_INET*, SOCK_DGRAM)`,
`AF_PACKET`, `ptrace`, `mount`.

**Namespaces** are rejected: unprivileged user namespaces are restricted by
AppArmor on Ubuntu 23.10+ (`kernel.apparmor_restrict_unprivileged_userns=1`)
and are routinely unavailable nested inside containers. Everything below is
designed so none are needed.

## Design

### No mounts: the private directories are env plus allowlist

Otto already constructs the child's environment, so the private
`HOME`/`TMPDIR`/cache do not need a mount namespace. Point the variables at
`PrivateDirectories` and let the Landlock ruleset allow only those paths plus
the workspace and the reviewed read roots. This single decision removes the
dependency on bubblewrap and on unprivileged user namespaces, and with them
the Ubuntu AppArmor problem and the nested-container problem.

A semantic detail makes this a clean port of the Seatbelt behavior:
**Seatbelt denies access, it does not hide files — and Landlock behaves
identically.** Moving from one to the other changes what a command is allowed
to touch, not what it observes.

### The ruleset is built before `fork`, applied after it

This is the load-bearing implementation constraint and the reason the driver
cannot simply call `rust-landlock` from a `pre_exec` closure.

Otto is a multithreaded tokio process. Between `fork` and `exec` the child may
call only async-signal-safe functions: another thread can hold the allocator
lock at the moment of the fork, and a `malloc` in the child then deadlocks it
forever. `rust-landlock`'s ruleset construction allocates.

So the driver splits the work:

1. **Parent, before `fork`:** create the ruleset, add every path rule, and
   keep the resulting ruleset file descriptor. All allocation happens here.
   Compile the seccomp filter into a `sock_fprog` here too, and keep the
   instruction buffer alive across the fork.
2. **Child, inside `pre_exec`:** raw syscalls only, in this order —
   `prctl(PR_SET_NO_NEW_PRIVS, 1, …)` (required by both mechanisms and by
   itself the defence against setuid re-entry),
   `seccomp(SECCOMP_SET_MODE_FILTER, …)`, `landlock_restrict_self(fd, 0)`.
   Any non-zero return aborts the child with a fixed exit code; it must
   never fall through to `exec` unconfined.

The child is the only thread in its own process at that point, so Landlock's
lack of TSYNC before ABI 8 is irrelevant. Process-group setup in
`sandbox/process.rs` is unchanged, and so is cancellation.

Denials are reported as `EACCES` (`SCMP_ACT_ERRNO`), not `SIGSYS`, so a
blocked command fails the way a permission error does rather than looking
like a crash — again matching Seatbelt.

### Capability tiers

Capabilities are computed from the detected ABI at open time, and
`supports_policy` turns them into the availability answer:

| Host | `read`/`write_confinement` | `unix_socket_deny` | `network_deny` | Confined policies supported |
| --- | --- | --- | --- | --- |
| No Landlock (or LSM disabled) | false | false | false | none — `landlock-missing` |
| ABI 1–3 (5.13–6.6) | true | true (seccomp) | false | `network = allow` only |
| ABI 4+ (6.7+) | true | true (seccomp) | true | both |

`unix_socket_deny` comes from seccomp refusing `socket(AF_UNIX, …)`, not from
Landlock ABI 6/9. That is the decision that keeps the kernel floor at 6.7 for
a fully confined session instead of 6.12, which would have excluded Ubuntu
24.04 (6.8) — see "Decisions to confirm".

`socketpair(AF_UNIX, …)` is a **different syscall and stays allowed**: it is
how ordinary build tooling makes an anonymous pipe pair, and blocking it would
break far more than it protects. Only named and abstract sockets are denied,
which is the part that can reach a host daemon.

### Default read allowlist

A confined child that cannot read `/usr/lib` cannot `exec` anything, so the
driver ships a fixed read-only base list — the system directories needed to
start a process (`/usr`, `/bin`, `/sbin`, `/lib*`, `/etc`), `/proc/self`, and
the character devices (`/dev/null`, `/dev/zero`, `/dev/urandom`, `/dev/tty`)
that need write as well. This list is part of the driver's contract and
belongs in a conformance check, not in a comment: a command that cannot run
`sh` is a sandbox bug, and it should fail a test rather than a user's session.

Otto's own `profiles`/state directories are simply absent from the allowlist,
the same way the Seatbelt profile excludes them.

## How it lands in the code

- `crates/otto/src/sandbox/landlock/`, `cfg(target_os = "linux")`, laid out
  like `sandbox/seatbelt/`: `driver.rs`, `ruleset.rs` (the pre-fork build),
  `seccomp.rs` (the filter), `selftest.rs`, `state.rs`. `sandbox/mod.rs` gates
  it the way it already gates `seatbelt`.
- `DriverMode` gains `Landlock`. `Auto` resolves per platform;
  `--sandbox seatbelt` on Linux and `--sandbox landlock` on macOS both stay
  errors rather than silently meaning something else.
- `UnavailableReason` gains `LandlockMissing` → `"landlock-missing"`,
  mirroring `SeatbeltMissing`, so `/sandbox` and the startup warning can say
  *why*. "Kernel too old for the requested network policy" needs no new
  variant: honest `Capabilities` plus `Error::UnsupportedPolicy` already map
  through `open_reason` to `policy-unsupported`.
- `cli/sandbox_runtime.rs`: `open_confined` gains a Linux arm alongside the
  macOS one; the non-macOS fallback that returns `UnsupportedPlatform` shrinks
  to the remaining platforms.
- A self-test at open, like Seatbelt's: run one probe child that must fail to
  write outside the workspace. It reports `SelfTestFailed`, which already
  exists.

## Testing

The conformance suite is the payoff and the acceptance criterion. A
`LandlockContract` implementing `Contract` inherits every existing check,
including the process-group and environment-replacement ones, and the
`advertised_*` checks self-skip against the capabilities the driver reports —
so one implementation covers both capability tiers.

New checks this design owes:

- the default read allowlist is sufficient to `exec` a shell;
- `socketpair` still works while `socket(AF_UNIX)` is denied;
- a `pre_exec` failure aborts the child instead of running it unconfined;
- with `network = deny`, a UDP send is refused as well as a TCP connect.

`Contract::skip_reason` reports the detected ABI so a runner on an older
kernel skips rather than fails. `make check-linux` gains the Landlock
conformance run, which is the first time that gate covers a confined driver.

## Honest gaps against macOS

These belong in the user manual, not only here:

1. **UDP and raw sockets are a seccomp coarse denial**, not a policy. Landlock
   covers TCP at ABI 4 and UDP only at ABI 10.
2. **Landlock cannot restrict procfs contents.** A confined child that can
   read `/proc` can see other processes' command lines. `ptrace` is blocked by
   seccomp; reading is not.
3. **No signal scoping below ABI 6.** A confined child can signal other
   processes of the same user. Otto's own teardown does not depend on this.
4. **No mDNSResponder analogue is needed** — the macOS exception exists only
   because `getaddrinfo` there talks to a daemon over a Unix socket.

## Decisions to confirm

1. **Unix sockets: seccomp `AF_UNIX` denial, or wait for Landlock ABI 6/9?**
   *Recommendation: seccomp.* It is fail-closed, works on every kernel, and
   keeps the confined floor at 6.7 (Ubuntu 24.04, Debian 13) instead of 6.12
   (Ubuntu 25.04+). The cost is coarseness: a command that legitimately wants
   a named Unix socket is refused rather than scoped.
2. **May `auto` select Landlock?** *Recommendation: yes, from the first
   release where the driver passes the conformance suite.* `Auto` already
   means "confine when the host supports it, otherwise fail closed", and
   opting in by hand would leave Linux users unconfined by default — the
   weaker outcome. `--sandbox off` remains the only way to run unconfined.
3. **Dependencies, or hand-written syscalls?** *Recommendation:
   `rust-landlock` plus `seccompiler`.* Landlock's best-effort/strict ABI
   negotiation is the exact thing that is easy to get subtly wrong by hand,
   and the crate is maintained by the feature's author. `seccompiler` is pure
   Rust with no C dependency. The alternative — hand-writing classic BPF as
   `session/fsops.rs` hand-writes `renameat2` — is defensible for a filter
   this small, but it needs correct architecture validation and 64-bit
   argument splitting, which is where hand-written filters are usually
   bypassable.

## Slices

Each slice is independently shippable and gated by the conformance suite.

1. **ABI detection and capability reporting, no enforcement.** Proves the
   fail-closed path end to end: on an old kernel the driver reports
   `landlock-missing`, `bash` is not registered, and `/sandbox` says why.
2. **Filesystem confinement** (ABI 1–3) plus the `AF_UNIX` seccomp denial.
   This is the slice that makes `network = allow` sessions usable.
3. **Network denial:** Landlock ABI 4 TCP rules plus the seccomp UDP and raw
   filter, lifting `network_deny`.
4. **Documentation of the remaining gaps** in the user manual, and a decision
   on whether ABI 6 signal scoping and ABI 10 UDP are worth adopting when
   those kernels are common.

## Rejected alternatives

- **bubblewrap / nsjail / minijail** — mature, but an external binary plus
  unprivileged user namespaces, which is precisely what is unreliable on
  modern Ubuntu and inside containers.
- **`systemd-run --user`** — `ProtectHome=`, `ReadOnlyPaths=` and
  `IPAddressDeny=` are the closest declarative analogue to a Seatbelt profile,
  but it requires a systemd user session, which containers and WSL often lack.
- **firejail** — a setuid binary with a history of privilege-escalation CVEs.
- **Docker / gVisor** — heavyweight, and out of scope per AGENTS.md.
