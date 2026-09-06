export type WebCommand =
  | { kind: 'prompt'; text: string }
  | { kind: 'rename'; name: string }
  | { kind: 'error'; message: string }

export function parseWebCommand(text: string): WebCommand {
  const trimmed = text.trim()
  if (!trimmed.startsWith('/')) return { kind: 'prompt', text }

  const match = /^\/rename(?:\s+(.*))?$/.exec(trimmed)
  if (!match) return { kind: 'prompt', text: trimmed }

  const name = (match[1] ?? '').trim()
  if (!name) return { kind: 'error', message: 'usage: /rename <name>' }
  return { kind: 'rename', name }
}
