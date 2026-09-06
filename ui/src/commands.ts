export type WebCommand =
  | { kind: 'prompt'; text: string }
  | { kind: 'help' }
  | { kind: 'session' }
  | { kind: 'new' }
  | { kind: 'resume' }
  | { kind: 'model' }
  | { kind: 'rename'; name: string }
  | { kind: 'compact'; focus: string }
  | { kind: 'sandbox' }
  | { kind: 'sandboxReload' }
  | { kind: 'tasks' }
  | { kind: 'task'; id: string }
  | { kind: 'taskCancel'; id: string }
  | { kind: 'exit' }
  | { kind: 'error'; message: string }

export interface WebCommandSuggestion {
  name: string
  description: string
}

export const supportedCommands: WebCommandSuggestion[] = [
  { name: '/help', description: 'show web commands' },
  { name: '/session', description: 'show current session details' },
  { name: '/new', description: 'start a new session' },
  { name: '/resume', description: 'resume a session from the picker' },
  { name: '/model', description: 'show current model and configured profiles' },
  { name: '/rename', description: 'rename the current session' },
  { name: '/compact', description: 'compact context with optional focus' },
  { name: '/sandbox', description: 'show sandbox state, or reload configuration' },
  { name: '/tasks', description: 'list sub-agent tasks' },
  { name: '/task', description: 'show or cancel a sub-agent task' },
  { name: '/exit', description: 'close the browser tab' },
]

export function webCommandSuggestions(text: string): WebCommandSuggestion[] {
  const trimmedStart = text.trimStart()
  if (!trimmedStart.startsWith('/') || /\s/.test(trimmedStart)) return []
  return supportedCommands.filter((command) => command.name.startsWith(trimmedStart))
}

export function parseWebCommand(text: string): WebCommand {
  const trimmed = text.trim()
  if (!trimmed.startsWith('/')) return { kind: 'prompt', text }

  const nameMatch = /^(\/\S+)(?:\s+(.*))?$/.exec(trimmed)
  const command = nameMatch?.[1] ?? trimmed
  const argument = (nameMatch?.[2] ?? '').trim()

  switch (command) {
    case '/help':
      return argument ? { kind: 'prompt', text: trimmed } : { kind: 'help' }
    case '/session':
      return argument ? { kind: 'prompt', text: trimmed } : { kind: 'session' }
    case '/new':
      return argument ? { kind: 'prompt', text: trimmed } : { kind: 'new' }
    case '/resume':
      return argument ? { kind: 'prompt', text: trimmed } : { kind: 'resume' }
    case '/model':
      return argument ? { kind: 'prompt', text: trimmed } : { kind: 'model' }
    case '/rename':
      return argument ? { kind: 'rename', name: argument } : { kind: 'error', message: 'usage: /rename <name>' }
    case '/compact':
      return { kind: 'compact', focus: argument }
    case '/sandbox':
      if (!argument) return { kind: 'sandbox' }
      if (argument === 'reload') return { kind: 'sandboxReload' }
      return { kind: 'prompt', text: trimmed }
    case '/tasks':
      return argument ? { kind: 'prompt', text: trimmed } : { kind: 'tasks' }
    case '/task': {
      const parts = argument.split(/\s+/).filter(Boolean)
      if (parts.length === 2 && parts[0] === 'cancel') return { kind: 'taskCancel', id: parts[1] }
      if (parts.length === 1 && parts[0] !== 'cancel') return { kind: 'task', id: parts[0] }
      return { kind: 'error', message: 'usage: /task <id|name> | /task cancel <id|name>' }
    }
    case '/exit':
      return argument ? { kind: 'prompt', text: trimmed } : { kind: 'exit' }
    default:
      return { kind: 'prompt', text: trimmed }
  }
}
