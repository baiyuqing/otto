import { initPrompt } from 'otto-web'
import { describe, expect, it } from 'vitest'
import { parseWebCommand, webCommandSuggestions } from './commands'

describe('web slash commands', () => {
  it('parses commands backed by existing server APIs', () => {
    expect(parseWebCommand('/help')).toEqual({ kind: 'help' })
    expect(parseWebCommand('/init')).toEqual({
      kind: 'prompt',
      text: initPrompt(),
    })
    expect(parseWebCommand('/session')).toEqual({ kind: 'session' })
    expect(parseWebCommand('/new')).toEqual({ kind: 'new' })
    expect(parseWebCommand('/clear')).toEqual({ kind: 'new' })
    expect(parseWebCommand('/resume')).toEqual({ kind: 'resume' })
    expect(parseWebCommand('/model')).toEqual({ kind: 'model' })
    expect(parseWebCommand('/compact')).toEqual({ kind: 'compact', focus: '' })
    expect(parseWebCommand('/compact focus on auth')).toEqual({ kind: 'compact', focus: 'focus on auth' })
    expect(parseWebCommand('/reflect')).toEqual({ kind: 'reflect', focus: '' })
    expect(parseWebCommand('/reflect the editor setup')).toEqual({ kind: 'reflect', focus: 'the editor setup' })
    expect(parseWebCommand('/skill generated')).toEqual({ kind: 'generatedSkills' })
    expect(parseWebCommand('/skill revert lint-gate')).toEqual({ kind: 'revertSkill', name: 'lint-gate' })
    expect(parseWebCommand('/sandbox')).toEqual({ kind: 'sandbox' })
    expect(parseWebCommand('/sandbox reload')).toEqual({ kind: 'sandboxReload' })
    expect(parseWebCommand('/approve approval-1')).toEqual({ kind: 'approve', id: 'approval-1' })
    expect(parseWebCommand('/deny approval-1')).toEqual({ kind: 'deny', id: 'approval-1' })
    expect(parseWebCommand('/tasks')).toEqual({ kind: 'tasks' })
    expect(parseWebCommand('/task t1')).toEqual({ kind: 'task', id: 't1' })
    expect(parseWebCommand('/task cancel t1')).toEqual({ kind: 'taskCancel', id: 't1' })
    expect(parseWebCommand('/mcp')).toEqual({ kind: 'mcp' })
    expect(parseWebCommand('/exit')).toEqual({ kind: 'exit' })
  })

  it('explains the /skill usage instead of sending a malformed command to the model', () => {
    const usage = { kind: 'error', message: 'usage: /skill generated | /skill revert <name>' }
    for (const text of ['/skill', '/skill revert', '/skill revert a b', '/skill generated now', '/skill other']) {
      expect(parseWebCommand(text)).toEqual(usage)
    }
  })

  it('leaves /init with arguments as a literal prompt', () => {
    expect(parseWebCommand('/init now')).toEqual({ kind: 'prompt', text: '/init now' })
  })

  it('falls through to a prompt when /mcp has an argument', () => {
    expect(parseWebCommand('/mcp x')).toEqual({ kind: 'prompt', text: '/mcp x' })
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
    expect(parseWebCommand('/approve')).toEqual({ kind: 'error', message: 'usage: /approve <id>' })
    expect(parseWebCommand('/deny')).toEqual({ kind: 'error', message: 'usage: /deny <id>' })
  })

  it('leaves non-commands and unsupported commands as prompts', () => {
    expect(parseWebCommand('hello')).toEqual({ kind: 'prompt', text: 'hello' })
    expect(parseWebCommand('/renam dev')).toEqual({ kind: 'prompt', text: '/renam dev' })
    expect(parseWebCommand('/memory search vim')).toEqual({ kind: 'prompt', text: '/memory search vim' })
  })

  it('suggests matching commands for slash prefixes', () => {
    expect(webCommandSuggestions('/')).toEqual([
      { name: '/help', description: 'show web commands' },
      { name: '/init', description: 'create a repository AGENTS.md guide' },
      { name: '/session', description: 'show current session details' },
      { name: '/new', description: 'start a new session' },
      { name: '/clear', description: 'start a new session' },
      { name: '/resume', description: 'resume a session from the sidebar' },
      { name: '/model', description: 'show current model and configured profiles' },
      { name: '/rename', description: 'rename the current session' },
      { name: '/compact', description: 'compact context with optional focus' },
      { name: '/reflect', description: 'queue memory candidates and skills from this session, with optional focus' },
      { name: '/skill', description: 'list skills reflection wrote, or undo one: generated | revert <name>' },
      { name: '/sandbox', description: 'show sandbox state, or reload configuration' },
      { name: '/approve', description: 'allow the elevated Bash command a running turn is waiting on' },
      { name: '/deny', description: 'deny the elevated Bash command a running turn is waiting on' },
      { name: '/tasks', description: 'list sub-agent tasks' },
      { name: '/task', description: 'show or cancel a sub-agent task' },
      { name: '/mcp', description: 'list MCP servers and their state' },
      { name: '/exit', description: 'close the browser tab' },
    ])
    expect(webCommandSuggestions('/r')).toEqual([
      { name: '/resume', description: 'resume a session from the sidebar' },
      { name: '/rename', description: 'rename the current session' },
      { name: '/reflect', description: 'queue memory candidates and skills from this session, with optional focus' },
    ])
    expect(webCommandSuggestions('/cl')).toEqual([
      { name: '/clear', description: 'start a new session' },
    ])
    expect(webCommandSuggestions('hello')).toEqual([])
  })
})
