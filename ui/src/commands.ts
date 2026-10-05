import { initPrompt } from 'otto-web'

export type WebCommand =
  | { kind: 'prompt'; text: string }
  | { kind: 'help' }
  | { kind: 'session' }
  | { kind: 'new' }
  | { kind: 'resume' }
  | { kind: 'model' }
  | { kind: 'rename'; name: string }
  | { kind: 'compact'; focus: string }
  | { kind: 'reflect'; focus: string }
  | { kind: 'reflectionRuns' }
  | { kind: 'reflectionRun'; id: string }
  | { kind: 'generatedSkills' }
  | { kind: 'revertSkill'; name: string }
  | { kind: 'auto'; automatic?: boolean }
  | { kind: 'sandbox' }
  | { kind: 'sandboxReload' }
  | { kind: 'approve'; id: string }
  | { kind: 'deny'; id: string }
  | { kind: 'tasks' }
  | { kind: 'task'; id: string }
  | { kind: 'taskCancel'; id: string }
  | { kind: 'mcp' }
  | { kind: 'exit' }
  | { kind: 'error'; message: string }

export interface WebCommandSuggestion {
  name: string
  description: string
}

export const supportedCommands: WebCommandSuggestion[] = [
  { name: '/help', description: 'show web commands' },
  { name: '/init', description: 'create a repository AGENTS.md guide' },
  { name: '/session', description: 'show current session details' },
  { name: '/new', description: 'start a new session' },
  { name: '/clear', description: 'start a new session' },
  { name: '/resume', description: 'resume a session from the sidebar' },
  { name: '/model', description: 'show current model and configured profiles' },
  { name: '/rename', description: 'rename the current session' },
  { name: '/compact', description: 'compact context with optional focus' },
  { name: '/reflect', description: 'retrospective plus memory and skill proposals, with optional focus; list | show <id> for past runs' },
  { name: '/skill', description: 'list skills reflection wrote, or undo one: generated | revert <name>' },
  { name: '/auto', description: 'show approval mode, or set on/off for this session' },
  { name: '/sandbox', description: 'show sandbox state, or reload configuration' },
  { name: '/approve', description: 'allow the elevated Bash command a running turn is waiting on' },
  { name: '/deny', description: 'deny the elevated Bash command a running turn is waiting on' },
  { name: '/tasks', description: 'list sub-agent tasks' },
  { name: '/task', description: 'show or cancel a sub-agent task' },
  { name: '/mcp', description: 'list MCP servers and their state' },
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
    case '/init':
      return argument ? { kind: 'prompt', text: trimmed } : { kind: 'prompt', text: initPrompt() }
    case '/session':
      return argument ? { kind: 'prompt', text: trimmed } : { kind: 'session' }
    case '/new':
    case '/clear':
      return argument ? { kind: 'prompt', text: trimmed } : { kind: 'new' }
    case '/resume':
      return argument ? { kind: 'prompt', text: trimmed } : { kind: 'resume' }
    case '/model':
      return argument ? { kind: 'prompt', text: trimmed } : { kind: 'model' }
    case '/rename':
      return argument ? { kind: 'rename', name: argument } : { kind: 'error', message: 'usage: /rename <name>' }
    case '/compact':
      return { kind: 'compact', focus: argument }
    case '/reflect': {
      // As in the terminal: `list` and `show <id>` read run logs, and any
      // other text is the focus of a new run.
      const parts = argument.split(/\s+/).filter(Boolean)
      if (parts.length === 1 && parts[0] === 'list') return { kind: 'reflectionRuns' }
      if (parts[0] === 'show' && parts.length === 1) {
        return { kind: 'error', message: 'usage: /reflect [focus] | /reflect list | /reflect show <id>' }
      }
      if (parts[0] === 'show' && parts.length === 2) return { kind: 'reflectionRun', id: parts[1] }
      return { kind: 'reflect', focus: argument }
    }
    case '/skill': {
      const parts = argument.split(/\s+/).filter(Boolean)
      if (parts.length === 1 && parts[0] === 'generated') return { kind: 'generatedSkills' }
      if (parts.length === 2 && parts[0] === 'revert') return { kind: 'revertSkill', name: parts[1] }
      return { kind: 'error', message: 'usage: /skill generated | /skill revert <name>' }
    }
    case '/auto':
      if (!argument) return { kind: 'auto' }
      if (argument === 'on' || argument === 'off') return { kind: 'auto', automatic: argument === 'on' }
      return { kind: 'error', message: 'usage: /auto [on|off]' }
    case '/sandbox':
      if (!argument) return { kind: 'sandbox' }
      if (argument === 'reload') return { kind: 'sandboxReload' }
      return { kind: 'prompt', text: trimmed }
    case '/approve':
      return argument && !/\s/.test(argument)
        ? { kind: 'approve', id: argument }
        : { kind: 'error', message: 'usage: /approve <id>' }
    case '/deny':
      return argument && !/\s/.test(argument)
        ? { kind: 'deny', id: argument }
        : { kind: 'error', message: 'usage: /deny <id>' }
    case '/tasks':
      return argument ? { kind: 'prompt', text: trimmed } : { kind: 'tasks' }
    case '/task': {
      const parts = argument.split(/\s+/).filter(Boolean)
      if (parts.length === 2 && parts[0] === 'cancel') return { kind: 'taskCancel', id: parts[1] }
      if (parts.length === 1 && parts[0] !== 'cancel') return { kind: 'task', id: parts[0] }
      return { kind: 'error', message: 'usage: /task <id|name> | /task cancel <id|name>' }
    }
    case '/mcp':
      return argument ? { kind: 'prompt', text: trimmed } : { kind: 'mcp' }
    case '/exit':
      return argument ? { kind: 'prompt', text: trimmed } : { kind: 'exit' }
    default:
      return { kind: 'prompt', text: trimmed }
  }
}
