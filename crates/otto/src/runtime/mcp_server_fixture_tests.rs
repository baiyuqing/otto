//! A stdio MCP server fixture in `/bin/sh`, shared by the runtime tests.

use crate::mcp;
use otto_core::config::McpServerRuntime;
use otto_core::config::McpTransport;

/// A stdio server in `/bin/sh` that answers the legacy handshake and lists
/// one tool named `echo`. It uses only shell builtins, because the child's
/// environment has no `PATH`.
pub(crate) fn shell_mcp_server(name: &str, cwd: &std::path::Path) -> McpServerRuntime {
    shell_mcp_server_with_start_log(name, cwd, None)
}

/// [`shell_mcp_server`] whose process first appends its pid and a newline to
/// `start_log`, so a test counts how many processes were spawned.
pub(crate) fn shell_mcp_server_with_start_log(
    name: &str,
    cwd: &std::path::Path,
    start_log: Option<&std::path::Path>,
) -> McpServerRuntime {
    let start_line = start_log
        .map(|path| format!("echo $$ >> '{}'\n", path.display()))
        .unwrap_or_default();
    let script = format!(
        r#"{start_line}while IFS= read -r line; do
  id=${{line#*'"id":'}}; id=${{id%%[,\}}]*}}
  case "$line" in
    *'"method":"server/discover"'*) printf '{{"jsonrpc":"2.0","id":%s,"error":{{"code":-32601,"message":"no"}}}}\n' "$id";;
    *'"method":"initialize"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"{}"}}}}\n' "$id";;
    *'"method":"tools/list"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":[{{"name":"echo","inputSchema":{{"type":"object"}}}}]}}}}\n' "$id";;
  esac
done"#,
        mcp::LEGACY_VERSION
    );
    McpServerRuntime {
        name: name.to_string(),
        enabled: true,
        transport: McpTransport::Stdio {
            command: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script],
            env: Vec::new(),
            cwd: cwd.to_string_lossy().into_owned(),
        },
        secrets: Vec::new(),
    }
}
