import { describe, expect, it } from 'vitest'
import { parseWebCommand } from './commands'

describe('web slash commands', () => {
  it('parses rename commands with a trimmed name', () => {
    expect(parseWebCommand('/rename dev')).toEqual({ kind: 'rename', name: 'dev' })
    expect(parseWebCommand('  /rename   dev session  ')).toEqual({ kind: 'rename', name: 'dev session' })
  })

  it('returns a usage error for missing rename names', () => {
    expect(parseWebCommand('/rename')).toEqual({ kind: 'error', message: 'usage: /rename <name>' })
    expect(parseWebCommand('/rename   ')).toEqual({ kind: 'error', message: 'usage: /rename <name>' })
  })

  it('leaves non-commands and unknown commands as prompts', () => {
    expect(parseWebCommand('hello')).toEqual({ kind: 'prompt', text: 'hello' })
    expect(parseWebCommand('/renam dev')).toEqual({ kind: 'prompt', text: '/renam dev' })
  })
})
