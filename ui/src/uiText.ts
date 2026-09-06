export const workspaceName = (workspace: string) => {
  const trimmed = workspace.replace(/\/+$/, '')
  if (!trimmed) return workspace
  const parts = trimmed.split('/')
  return parts[parts.length - 1] || workspace
}

export const sessionLabel = (id: string) => `#${id.slice(0, 8)}`

export const sendHint = (isMac = navigator.platform.toLowerCase().includes('mac')) =>
  isMac ? 'Enter to send · Shift+Enter newline · ⌘+Enter also works' : 'Enter to send · Shift+Enter newline · Ctrl+Enter also works'
