export const workspaceName = (workspace: string) => {
  const trimmed = workspace.replace(/\/+$/, '')
  if (!trimmed) return workspace
  const parts = trimmed.split('/')
  return parts[parts.length - 1] || workspace
}

export const sessionLabel = (id: string) => `#${id.slice(0, 8)}`

export const sendHint = (isMac = navigator.platform.toLowerCase().includes('mac')) =>
  isMac ? 'Enter send · Shift+Enter newline · ⌘V paste image' : 'Enter send · Shift+Enter newline · Ctrl+V paste image'

export const mcpServerLine = (server: { name: string; transport: string; protocol_version: string | null; state: string; tools: number; error: string | null }) => {
  const detail = server.state === 'connected' ? ` (${server.tools} tools)` : server.error ? `: ${server.error}` : ''
  const login = server.state === 'needs_login' ? ` - run: kite mcp login ${server.name}` : ''
  return `${server.name}: ${server.state}${detail} (${server.transport}, ${server.protocol_version ?? '-'})${login}`
}
