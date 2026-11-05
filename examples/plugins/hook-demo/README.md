# Hook and custom tool example

Requires Python 3 on `PATH`. No Python packages or service credentials are
needed.

From the repository root:

```sh
otto plugin install ./examples/plugins/hook-demo
otto plugin inspect hook-demo
otto plugin trust hook-demo
```

Restart affected Otto runtimes to load the installed package, then start a new
session in any workspace. Use your usual configured provider.

Ask Otto:

> Call plugin__hook-demo__workspace_info with label "demo".

The tool returns the workspace path, up to 20 sorted entry names and the total
entry count. It reads the selected workspace; it does not write files.

Then ask:

> Call plugin__hook-demo__workspace_info with label "blocked".

The `before_tool` hook rejects that label before the tool executes. This
sentinel demonstrates hook decisions; it is not a general security policy.
Both events use the same `demo.py`, which switches on the JSON `event` field.

Commands follow Otto's existing sandbox policy. Linux requires explicit
`--sandbox off` to run external commands. Editing the source example does not
change its installed snapshot: reinstall and trust the new digest.

Run the offline script check:

```sh
python3 examples/plugins/test_hook_demo.py
```

Disable or remove the example when finished:

```sh
otto plugin disable hook-demo
# Or: otto plugin remove hook-demo
```

Protocol: [Hooks and custom tools](../../../docs/manual/memory-and-extensions.md#hooks-and-custom-tools).
