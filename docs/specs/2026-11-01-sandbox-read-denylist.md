# Sandbox read policy: denylist instead of allowlist

Status: approved by the user, 2026-11-01.

## Rule: reads are denied by list, writes are allowed by list

A sandboxed `bash` command reads every path on the host except the paths on
the read denylist. The denylist is the built-in list in this document plus
`[sandbox] deny_read_paths`. `[sandbox] read_paths` names exceptions inside
the denylist. Writes are unchanged: the workspace, its linked worktrees and
the session's private `tmp` and `cache` directories.

The sandboxed command's `HOME` is the user's home directory. The private
per-session `home` directory is no longer created.

## Current state: an allowlist of read roots and a private `HOME`

`crates/otto/src/sandbox/seatbelt/profile/roots.rs` builds the readable set
from five sources: the writable set, 26 fixed system directories and 5 files
under `/private/etc` (`REVIEWED_AUTOMATIC_PATHS`, `REVIEWED_RUNTIME_FILES`),
the `xcode-select` developer directory, every directory on the host `PATH`,
and `[sandbox] read_paths`. The user's home directory is excluded by
`exact_broad_anchor`, and `crates/otto/src/sandbox/environment.rs:223` sets
`HOME` to a private per-session directory.

Consequences, measured on one user configuration on 2026-11-01:

| Date | `read_paths` entries |
| --- | --- |
| 2026-09-02 | 0 |
| 2026-09-27 | 7 |
| 2026-10-15 | 17 |
| 2026-11-01 | 23 |

Of the 23 entries, 6 are covered by another entry (`~/.config/gh` twice,
`/etc/ssl` and `/private/etc/ssl`, `.config/gh` and `.config/otto` under
`.config`, `.otto/memory` under `.otto`, `.cargo/registry` under `.cargo`), 5
belong to programs that `excluded_commands` already runs unconfined, and
`~/.otto` exposes `~/.otto/auth/chatgpt.json` to every sandboxed command. Each
entry is a permanent, global grant; no entry records why it was added. Tools
that locate their configuration through `$HOME` (git, cargo, rustup, gh) do
not find it even when the path is listed, because `HOME` is private.

## New profile: one global read allow, then the denies, then the exceptions

Seatbelt applies the last matching rule. The template
`testdata/sandbox/profile_v1.sb` changes as follows:

1. `(allow file-read*)` without a filter is added directly after
   `(deny default)`.
2. The existing `(deny file-read* (subpath "/System/Volumes"))` stays and
   must follow the global allow. Probe 3 shows that Seatbelt evaluates the
   firmlink-normalized path: `/System/Volumes/Data<denied path>` is refused
   by the deny on `<denied path>` itself, and `/System/Volumes/Data<readable
   path>` is readable. The rule adds nothing for Data-volume paths and is
   kept for paths that physically live below `/System/Volumes`.
3. The rules that existed only because reads were denied by default are
   removed: `(allow file-read-data (literal "/"))`, the `file-read-metadata`
   rules for `/private/var/select/sh`, `/var`, `/var/select`,
   `/private/var/select`, `*/developer_dir` and `/etc`, and the
   `file-read*` rule for `/dev/null`, `/dev/random`, `/dev/urandom`.
4. The `@@OTTO_PROFILE_READ_RULES@@` section renders, in this order:

```scheme
; OTTO-DYNAMIC-READ-DENY-BEGIN
(deny file-read*
  (regex "^<cache_base>/otto-sandbox-")   ; every otto private sandbox root
  (subpath "<home>/.ssh")                  ; built-in list, see below
  ...
  (subpath "<deny_read_paths entry>")      ; user additions
  ...
  (with message "<tag>"))
; OTTO-DYNAMIC-READ-DENY-END
; OTTO-DYNAMIC-READ-ALLOW-BEGIN
(allow file-read*
  (subpath "<own private root>/tmp")
  (subpath "<own private root>/cache")
  (subpath "<read_paths exception>")       ; only exceptions inside a deny
  ...)
; OTTO-DYNAMIC-READ-ALLOW-END
```

Every deny entry is rendered as `subpath`, which matches the path itself and
everything below it, so a file and a directory need no distinction and an
entry that does not exist yet is still covered. An entry that exists is
canonicalized; when the canonical spelling differs from the configured one,
both are rendered. `<cache_base>` in the regex is the canonical
`<home>/Library/Caches` with regex metacharacters escaped. The `profiles`
directory below the own private root stays denied because it is not among
the exceptions.

The write rules, network rules and shell rule are unchanged.

## Built-in denylist

Paths are relative to the user's home directory unless absolute. Directories
are preferred over single files so that a tool's new credential file is
covered without a list change.

Credentials and keys:

- `~/.ssh`
- `~/.gnupg`
- `~/.aws`
- `~/.azure`
- `~/.config/gcloud`
- `~/.kube`
- `~/.docker`
- `~/.netrc`
- `~/.git-credentials`
- `~/.npmrc`
- `~/.pypirc`
- `~/.cargo/credentials.toml`
- `~/.cargo/credentials`
- `~/.gem/credentials`
- `~/.m2/settings.xml`
- `~/.terraform.d/credentials.tfrc.json`
- `~/.config/gh`

Credentials of agent tools, including Otto's own:

- `~/.otto/auth`
- `~/.claude/.credentials.json`
- `~/.codex/auth.json`

Shell history:

- `~/.zsh_history`
- `~/.bash_history`
- `~/.zsh_sessions`

macOS personal data:

- `~/Library/Keychains`
- `~/Library/Cookies`
- `~/Library/Safari`
- `~/Library/Application Support/Google/Chrome`
- `~/Library/Application Support/Firefox`
- `~/Library/Messages`
- `~/Library/Mail`

Otto private sandbox state:

- every `otto-sandbox-*` root under `~/Library/Caches` (regex rule above)

A built-in entry that equals or contains a writable root (the workspace or a
linked worktree) is skipped, because the sandbox
cannot work without reading them, and is reported in the sandbox state. The
list is a constant in `crates/otto/src/sandbox/seatbelt/profile/` and is
documented verbatim in `docs/manual/safety-and-tools.md`. The documentation
states that paths not on the list are readable.

## Configuration: `deny_read_paths` is new, `read_paths` keeps its name

```toml
[sandbox]
driver = "auto"
network = "allow"
deny_read_paths = []   # added to the built-in denylist
read_paths = []        # exceptions: readable although the denylist covers them
allow_env = []
excluded_commands = []
```

`deny_read_paths`:

- Each entry is an absolute path or starts with `~/`; the same text rules as
  `read_paths` apply (`valid_sandbox_read_path`,
  `crates/otto-core/src/config/sandbox.rs:304`). Entries are sorted.
- An entry need not exist.
- An entry that equals or contains a writable root is rejected at sandbox
  construction with the existing single `Rejected` value, so the sandbox is
  unavailable rather than silently narrower. An entry below the workspace is
  accepted: reads of that subtree are denied while writes stay allowed.
- `deny_read_paths` takes effect on `/sandbox reload`, like `read_paths`.

`read_paths`:

- Same syntax and validation as today. An entry must exist when the profile
  is built; a missing entry is skipped, as today.
- An entry is rendered only when its canonical path equals or lies below a
  deny entry (built-in or configured). Such an entry is an *active*
  exception.
- An entry that is not below any deny entry is *inert*: it is accepted,
  changes nothing, and is listed in the sandbox state (next section). An
  entry that is an ancestor of a deny entry is inert too: `~/.otto` does not
  lift `~/.otto/auth`.
- An entry overlapping an otto private sandbox root is rejected, as today.

The native `otto::sandbox::Settings` gains `deny_read_paths: Vec<String>`.
`otto_core::config::SandboxSettings` and `resolve_sandbox` gain the same
field. `update_sandbox` and `otto sandbox setup` write the key.

## Sandbox state reports inert exceptions and skipped denies

`SandboxInfo` (`crates/otto/src/runtime/info.rs:97`) gains
`inert_read_paths: Vec<String>` and `skipped_deny_paths: Vec<String>`. The
`/sandbox` command without arguments prints them after the summary line, one
path per line, with the sentence "not below any denied path; remove it from
read_paths" or "contains a writable root; not applied". The HTTP `Sandbox`
object in `testdata/server/openapi.yaml` gains the two optional arrays. A
reload reports the same.

## HOME and environment

`resolve_environment` (`crates/otto/src/sandbox/environment.rs`) changes:

- `HOME` keeps the host value. The private `home` directory is no longer
  created; `PrivateDirectories` loses its `home` field and the Seatbelt
  profile no longer grants it.
- `ZDOTDIR` is set to the private `tmp` directory so that `zsh -c` does not
  source `~/.zshenv`, `~/.zprofile` or `~/.zshrc`. `BASH_ENV` and `ENV`
  stay withheld (shell-startup injection variables).
- `TMPDIR`, `TMP`, `TEMP`, `TMPPREFIX`, `XDG_CACHE_HOME`, `GOCACHE`,
  `GOMODCACHE`, `NPM_CONFIG_CACHE`, `PIP_CACHE_DIR` and `UV_CACHE_DIR` are
  unchanged and still point into the private directories.
- `CARGO_HOME` is set to `<cache>/cargo`, like the other cache redirects.
  `RUSTUP_HOME` is not set: the rustup proxy in `~/.cargo/bin` finds
  `~/.rustup` through the real `HOME`. Cargo therefore reads neither
  `~/.cargo/config.toml` nor `~/.cargo/credentials.toml` and downloads the
  registry into the per-session cache (known limitation below).

The `bash` tool description drops `PRIVATE_HOME_NOTE` ("HOME and ~ are a
private sandbox directory ..."), and `with_private_home` and the
`private_home` flag are removed.

## Runtime grants request exceptions, not readable paths

- `/sandbox allow <path>` and the model's `sandbox_read_path` argument keep
  their flow (`crates/otto/src/app/sandbox_approval.rs`,
  `crates/otto/src/tool/bash/execute.rs`). Both now require the resolved
  path to be below a deny entry. A path that is already readable is refused
  with the text `path is already readable; run the command without
  sandbox_read_path`. Persistent grants still require a person in AUTO.
- The `sandbox_denied:` section of a failed command keeps its format. Its
  trailing hint changes to: "a denied read is on the sandbox read denylist;
  request a persistent exception with sandbox_read_path, or an elevated run
  with sandbox_permissions".
- `otto sandbox setup`: the GitHub CLI recipe adds `~/.config/gh` (or the
  absolute `GH_CONFIG_DIR`) to `read_paths` as an exception to the built-in
  deny. It no longer adds `GH_CONFIG_DIR` to `allow_env` and no longer
  prints a launch command, because `gh` finds `~/.config/gh` through the
  real `HOME`. The check still runs `gh --version` and tests that the
  directory is readable.

## Removed code

From `crates/otto/src/sandbox/seatbelt/profile/`:

- `REVIEWED_AUTOMATIC_PATHS`, `REVIEWED_RUNTIME_FILES`, `AutomaticPath`,
  `resolve_automatic_path`, `safe_fixed_root`.
- `discover_developer_root` and `Dependencies::developer_root`.
- `path_entries` and `Options::host_entries`: the profile no longer reads
  the host `PATH`.
- `exact_broad_anchor`, `safe_automatic_directory_root`.
- `collapse_read_roots`, `roots_within_limits`, `MAX_DYNAMIC_ROOTS`,
  `metadata_ancestors`, and the `OTTO-DYNAMIC-METADATA` section of
  `render_read_rules`. `MAX_DYNAMIC_PATH_BYTES` stays: the path text of the
  deny and allow forms together is bounded at 32 KiB, and a longer
  configuration is `Rejected`.

`paths.rs` keeps `resolve_path`, `canonical_filesystem_path`,
`valid_path_text`, `clean`, `join`, `path_within`, `paths_overlap`,
`sort_paths`. `roots.rs` is renamed to `deny_roots.rs` and holds the
built-in list, the deny set construction and the exception filter.

From `crates/otto/src/runtime/startup/sandbox.rs`: `resolve_sandbox_settings`
no longer pushes existing skill roots, agent roots and plugin snapshot
package paths into `read_paths`. Under the denylist the global read allow
covers them, and the pushed entries would be reported as inert. A catalog
root that a configured `deny_read_paths` entry contains needs an explicit
`read_paths` exception. The tests
`sandbox_settings_pick_up_existing_skill_and_agent_roots` and
`a_symlinked_skill_root_is_not_promoted_to_a_sandbox_read_path` are removed
with the promotion.

## Compatibility

- Existing configurations load unchanged. `read_paths` entries that are
  not below a deny entry become inert and are reported; nothing is rewritten.
- Session history is untouched. The approval record for a read grant keeps
  its shape.
- The HTTP `Sandbox` object gains two optional arrays and loses nothing.
- Commands that relied on `~` being an empty writable directory now see the
  real home, which is read-only outside the writable set. A command that
  wrote under `~` fails with a write denial and the `sandbox_denied:` report
  names the path; `TMPDIR` remains the writable scratch location.

## Documentation and capability descriptions updated in the same PR

- `docs/manual/safety-and-tools.md`, section "bash sandbox policy": rewrite
  the read-policy paragraphs; list the built-in denylist verbatim; state
  that unlisted paths are readable and that a denylist is not complete.
- `docs/manual/getting-started.md`: the `[sandbox]` example gains
  `deny_read_paths = []` and a one-line comment per key.
- `testdata/server/openapi.yaml`: `/v1/sandbox/reload` description and the
  `Sandbox` object.
- `bash` tool description (`crates/otto/src/tool/bash.rs`): the capability
  description says "Sandboxed commands read every path except the sandbox
  read denylist (credential and personal-data locations)"; the
  `sandbox_read_path` description says "Request user approval to permanently
  allow reading one absolute existing path that the sandbox read denylist
  covers, before running this command inside the sandbox".
- `CHANGELOG.md`: one entry under Unreleased, Changed.
- `docs/code-organization.md`: `sandbox/` entry mentions the denylist.

## Tests

Probes, run on 2026-11-01 with `sandbox-exec` on macOS 27.2 against a
profile built as in the section above (script path in the plan file):

1. `(allow file-read*)` without a filter is accepted; a regular file outside
   the workspace is readable; `/etc/hosts` and `/usr/bin/true` work without
   any system-path rule.
2. `(deny file-read* (regex #"^<base>/otto-sandbox-"))` is accepted; a
   sibling root is refused; the own `tmp` exception rendered after it is
   readable; the own `profiles` directory is refused.
3. `/System/Volumes/Data<denied path>` is refused and
   `/System/Volumes/Data<readable path>` is readable: Seatbelt matches the
   firmlink-normalized path.
4. `ln <denied file> <writable dir>/x` is refused in every variant: with no
   `file-link` rule, with `(allow file-link)` unfiltered, and with
   `file-write*` allowed on the denied file's directory. `ln <readable file>
   <writable dir>/x` succeeds only when `file-write*` is also allowed on the
   source. Seatbelt requires read and write permission on the source of a
   hard link; a denied path is never writable in Otto's profile, so no
   `file-link` rule is needed.
5. A symlink from the writable directory to a denied file can be created;
   reading through it is refused.
6. `ls` and `stat` on a denied path are refused: `file-read*` covers
   metadata.

Unit tests, `crates/otto/src/sandbox/seatbelt/profile/tests.rs`:

- The rendered profile has `(allow file-read*)` before every `deny
  file-read*` rule, and the `/System/Volumes` deny after it.
- Every built-in entry is rendered below the resolved home.
- A `deny_read_paths` entry that contains the workspace is rejected; one
  below the workspace is rendered.
- A `read_paths` entry below a deny entry is rendered in the allow section;
  one outside every deny entry is not rendered and appears in the inert
  list; an ancestor of a deny entry is not rendered.
- The own `tmp` and `cache` directories are rendered in the allow section
  after the private-root regex deny; the `profiles` directory is not.
- A home path containing regex metacharacters is escaped in the regex rule.

Unit tests, `crates/otto/src/sandbox/environment/tests/`:

- `HOME` equals the host value; `ZDOTDIR` is the private `tmp` directory;
  `TMPDIR` is private; `CARGO_HOME` is below the private `cache` directory;
  `RUSTUP_HOME` is absent.

Unit tests, `crates/otto-core/src/config/sandbox.rs`:

- `deny_read_paths` parses, validates like `read_paths`, sorts, and
  survives `update_sandbox`.

Conformance checks, `crates/otto/src/sandbox/conformance_tests/`
(macOS, live Seatbelt):

- A regular file in a temporary directory outside the workspace is readable
  (fails on the current allowlist).
- A file below a configured `deny_read_paths` directory is unreadable, and
  the `sandbox_denied:` report names it.
- The same file is readable when its directory is in `read_paths`.
- A sibling `otto-sandbox-*` root and the own `profiles` directory are
  unreadable (existing check `sibling_process_environment_is_unreadable`
  is kept).
- `zsh -c 'echo $HOME'` prints the host home and does not source a
  `.zshenv` placed in the host home by the test (the test uses a controlled
  home directory).

Tests, `crates/otto/src/tool/bash/`:

- `sandbox_read_path` for a path outside the deny set returns the
  "already readable" argument error without creating an approval request.

## Known limitations

- A denylist protects listed locations only. A credential stored at an
  unlisted path is readable and, with `network = "allow"`, can be sent off
  the host. The built-in list is reviewed by category, not extended per
  incident.
- `deny_read_paths` entries are not matched against `excluded_commands`:
  an excluded command runs unconfined and reads everything, as today.
- Caches redirected into the private `cache` directory (`cargo`, `go`, `npm`,
  `pip`, `uv`) are per-session: each session downloads dependencies again.
  Cargo does not read `~/.cargo/config.toml` (registry mirrors, build
  settings) because `CARGO_HOME` is the private cache.
