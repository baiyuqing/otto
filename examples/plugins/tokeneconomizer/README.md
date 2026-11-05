# TokenEconomizer bridge example

Runs a local TokenEconomizer checkout as an external host of Otto. `bridge.py`
receives Otto hook protocol v1 events and passes them to TokenEconomizer's
`scripts/te-hook --host pi`, then converts the reply to an Otto hook response.
TokenEconomizer itself is not modified or vendored.

## Requirements

- Python 3.10 or later on `PATH`, for `bridge.py` and for TokenEconomizer.
- A local TokenEconomizer checkout. The bridge runs `plugin/scripts/te-hook`
  from it.

## Install

From the repository root:

```sh
otto plugin install ./examples/plugins/tokeneconomizer
otto plugin inspect tokeneconomizer
otto plugin trust tokeneconomizer
```

Editing the source example does not change its installed snapshot: reinstall
and trust the new digest.

## Native install without the bridge

Otto reads Claude Code hooks from a `claude` package
([Claude Code hook format](../../../docs/manual/memory-and-extensions.md#claude-code-hook-format)),
so a TokenEconomizer checkout can be installed directly, without `bridge.py`:

```sh
otto plugin install /path/to/TokenEconomizer/plugin
otto plugin inspect tokeneconomizer
otto plugin trust tokeneconomizer
```

The path is the plugin directory that contains `.claude-plugin/plugin.json`.
The package name is the `name` field of that manifest, `tokeneconomizer`, the
same name as the bridge package above. `otto plugin install` refuses a name
that is already installed, so remove one package before installing the other.
The package's
`te-hook` hooks for `SessionStart`, `UserPromptSubmit`, `PostToolUse`,
`PostToolUseFailure`, `Stop` and `SessionEnd` then run natively: micro-skill
injection at session start, the loop guard on prompts and tool results, and the
turn close on `Stop`. Otto sets `CLAUDE_CODE_SESSION_ATTENDED` (`0` for
`otto --prompt`, else `1`).

Limitation: TokenEconomizer's Claude host reads `transcript_path` as a Claude
transcript, and Otto writes its own JSONL file there. Transcript-based learning
records nothing, and micro-skills that depend on the transcript still need the
bridge.

## Configuration

Export the checkout's `plugin` directory before starting Otto:

```sh
export TE_PLUGIN_ROOT=/path/to/TokenEconomizer/plugin
```

Add the binding to the Otto configuration file:

```toml
[plugins.bindings.tokeneconomizer]
env = { TE_PLUGIN_ROOT = "${TE_PLUGIN_ROOT}" }
```

- A binding value may also be a literal path
  (`TE_PLUGIN_ROOT = "/path/to/TokenEconomizer/plugin"`); only values that
  look like credentials are rejected.
- The default sandbox read denylist does not block an ordinary checkout or
  `~/.otto/sessions`; neither needs a blanket `read_paths` grant. If your
  checkout or configured session-storage route is under an additional denied
  path, diagnose the actual refusal and grant only the required directory
  ([sandbox policy](../../../docs/manual/safety-and-tools.md#bash-sandbox-policy)).
- Restart Otto after changing the configuration, then start a new session.

## What runs on each event

Each event runs `python3 bridge.py`, which runs `te-hook --host pi` in the
workspace with the Claude-shaped event the Pi extension would send.

| Otto event | TokenEconomizer event | Reply handling |
| --- | --- | --- |
| `session_start` | `SessionStart` (`source` is `startup` for a new session, else `resume`) | `additionalContext` becomes `context`; `systemMessage` becomes `notice` |
| `before_prompt` | `UserPromptSubmit` | same as `session_start` |
| `after_tool` | `PostToolUse`, with the tool name, arguments, output and exit code | `additionalContext` is appended to the tool result after a blank line; `systemMessage` becomes `notice` |
| `turn_end` | `Stop`, sent only when the turn status is `success` | `systemMessage` becomes `notice`; context is dropped |
| `session_end` | `SessionEnd`, which starts a detached fold of the session file | context is dropped; the notice is not displayed |

The exit code comes from the `exit_code: N` line in the bash tool's result and
is omitted when that line is absent. `te-hook` runs with `PYTHONUTF8=1`;
`TE_PI_MODE=print` when Otto runs with `--prompt` and the variable is unset;
and `TOKENECONOMIZER_HOME=<workspace>/.otto/tokeneconomizer` when unset.

The bridge never fails a hook. A missing `TE_PLUGIN_ROOT`, a missing `te-hook`,
a timeout, a non-zero exit or output without a JSON object produce empty
stdout, one line on stderr and exit status 0.

## See what it learned

TokenEconomizer keeps its state under `.otto/tokeneconomizer` in the
workspace. Run its command line with the same environment variable:

```sh
export TOKENECONOMIZER_HOME="$PWD/.otto/tokeneconomizer"
"$TE_PLUGIN_ROOT/scripts/te" --host pi status
"$TE_PLUGIN_ROOT/scripts/te" --host pi microskills
```

## Offline check

```sh
python3 examples/plugins/test_tokeneconomizer_bridge.py
```

The check uses a fake `te-hook` and needs neither TokenEconomizer nor Otto.

## Limitations

- Sandboxed hooks can write only the workspace and a per-session private home
  that is removed on close. `~/.tokeneconomizer` and `PLUGIN_DATA` are not
  writable, so state defaults to `<workspace>/.otto/tokeneconomizer`. A
  per-package writable grant is possible follow-up work.
- This list applies to the bridge. The native install is described in
  [Native install without the bridge](#native-install-without-the-bridge).
- Whether the detached `te fold` process survives the end of the hook process
  under the sandbox has not been verified.
- TokenEconomizer's notice texts refer to Pi (`/te microskills`, "Press Esc to
  abort it."). State is shared with Pi sessions under `repos/<key>/pi`.
- Side calls (`pi -p`) are unavailable, so the loop-guard judge stays paused.
  Otto's `write` tool is not counted as an edit by the loop guard, which counts
  `edit` and `create`.
- Facts injected at `session_start` are lost after compaction, because
  `after_compaction` accepts no `context`.
- `session_end` notices are not displayed.
- Otto's `session_end` carries no reason, so the bridge sends `quit`. Turns
  with status `error` or `cancelled` send nothing; TokenEconomizer closes the
  open turn at the next `UserPromptSubmit`.

Protocol: [Hooks and custom tools](../../../docs/manual/memory-and-extensions.md#hooks-and-custom-tools).
