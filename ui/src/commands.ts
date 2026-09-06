export type WebCommand =
  | { kind: 'prompt'; text: string }
  | { kind: 'rename'; name: string }
  | { kind: 'error'; message: string }

export interface WebCommandSuggestion {
  name: string
  description: string
}

const supportedCommands: WebCommandSuggestion[] = [
  { name: '/rename', description: 'rename the current session' },
  { name: '/compact', description: 'compact context with optional focus' },
]

export function webCommandSuggestions(text: string): WebCommandSuggestion[] {
  const trimmedStart = text.trimStart()
  if (!trimmedStart.startsWith('/') || /\s/.test(trimmedStart)) return []
  return supportedCommands.filter((command) => command.name.startsWith(trimmedStart))
}

export function parseWebCommand(text: string): WebCommand {
  const trimmed = text.trim()
  if (!trimmed.startsWith('/')) return { kind: 'prompt', text }

  const match = /^\/rename(?:\s+(.*))?$/.exec(trimmed)
  if (!match) return { kind: 'prompt', text: trimmed }

  const name = (match[1] ?? '').trim()
  if (!name) return { kind: 'error', message: 'usage: /rename <name>' }
  return { kind: 'rename', name }
}
