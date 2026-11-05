# Built-in Lua hook and tool

No Lua executable, Python, package manager, credentials or network service is
required. Otto includes the Lua 5.4 runtime by default. From the repository root:

```sh
otto plugin install ./examples/plugins/lua-demo
otto plugin inspect lua-demo
otto plugin trust lua-demo
```

Review the manifest and `calculate.lua` before trusting. Restart long-lived Otto
runtimes/start a new session, then ask:

> Call plugin__lua-demo__calculate with number 21.

The existing tool approval flow applies; the result is `42`. Request number `-1`
to see the `before_tool` hook deny the call. Hook decisions cannot grant approval.
The handler performs only calculation; even a trusted Lua script has no host
filesystem, environment, processes, network or direct Otto tool access. Pure Lua
calls do not need `--sandbox off` because they spawn no process. Shell/MCP plugins
retain their own existing execution rules.

To update, edit the source package, remove/reinstall the package, inspect and
trust its new digest, then restart affected runtimes. Old runners keep their
pinned snapshot, but `otto plugin revoke lua-demo` immediately blocks subsequent
calls and cooperatively stops running Lua calls. `disable` affects new runners.

Missing/invalid source paths, source bytecode and conflicting `command`/`lua`
handlers fail installation. Syntax/runtime/resource errors fail the call without
echoing the script's arbitrary error text. Check source syntax, the existing
response protocol and documented limits; fix, reinstall, inspect, retrust and
restart rather than weakening sandbox or approvals.

See the [Lua handler manual](../../../docs/manual/memory-and-extensions.md#lua-handlers)
and [security contract](../../../docs/development/lua-plugin-security.md). Lua is
an in-process restricted VM, not OS isolation; deadlines are cooperative.
