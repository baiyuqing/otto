import { describe, expect, it } from 'vitest'
import { parseWebCommand, webCommandSuggestions } from './commands'

describe('web slash commands', () => {
  it('parses commands backed by existing server APIs', () => {
    expect(parseWebCommand('/help')).toEqual({ kind: 'help' })
    expect(parseWebCommand('/session')).toEqual({ kind: 'session' })
    expect(parseWebCommand('/new')).toEqual({ kind: 'new' })
    expect(parseWebCommand('/resume')).toEqual({ kind: 'resume' })
    expect(parseWebCommand('/model')).toEqual({ kind: 'model' })
    expect(parseWebCommand('/compact')).toEqual({ kind: 'compact', focus: '' })
    expect(parseWebCommand('/compact focus on auth')).toEqual({ kind: 'compact', focus: 'focus on auth' })
    expect(parseWebCommand('/sandbox')).toEqual({ kind: 'sandbox' })
    expect(parseWebCommand('/sandbox reload')).toEqual({ kind: 'sandboxReload' })
    expect(parseWebCommand('/tasks')).toEqual({ kind: 'tasks' })
    expect(parseWebCommand('/task t1')).toEqual({ kind: 'task', id: 't1' })
    expect(parseWebCommand('/task cancel t1')).toEqual({ kind: 'taskCancel', id: 't1' })
    expect(parseWebCommand('/exit')).toEqual({ kind: 'exit' })
  })

  it('parses rename commands with a trimmed name', () => {
    expect(parseWebCommand('/rename dev')).toEqual({ kind: 'rename', name: 'dev' })
    expect(parseWebCommand('  /rename   dev session  ')).toEqual({ kind: 'rename', name: 'dev session' })
  })

  it('returns usage errors for missing command arguments', () => {
    expect(parseWebCommand('/rename')).toEqual({ kind: 'error', message: 'usage: /rename <name>' })
    expect(parseWebCommand('/rename   ')).toEqual({ kind: 'error', message: 'usage: /rename <name>' })
    expect(parseWebCommand('/task')).toEqual({ kind: 'error', message: 'usage: /task <id|name> | /task cancel <id|name>' })
    expect(parseWebCommand('/task cancel')).toEqual({ kind: 'error', message: 'usage: /task <id|name> | /task cancel <id|name>' })
  })

  it('leaves non-commands and unsupported commands as prompts', () => {
    expect(parseWebCommand('hello')).toEqual({ kind: 'prompt', text: 'hello' })
    expect(parseWebCommand('/renam dev')).toEqual({ kind: 'prompt', text: '/renam dev' })
    expect(parseWebCommand('/memory search vim')).toEqual({ kind: 'prompt', text: '/memory search vim' })
  })

  it('suggests matching commands for slash prefixes', () => {
    expect(webCommandSuggestions('/')).toEqual([
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
    ])
    expect(webCommandSuggestions('/r')).toEqual([
      { name: '/resume', description: 'resume a session from the picker' },
      { name: '/rename', description: 'rename the current session' },
    ])
    expect(webCommandSuggestions('hello')).toEqual([])
  })
})
