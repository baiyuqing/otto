import { describe, expect, it } from 'vitest'
import { sendHint, sessionLabel, workspaceName } from './uiText'

describe('ui text helpers', () => {
  it('formats compact labels for the shell around the transcript', () => {
    expect(workspaceName('/Users/me/work/otto')).toBe('otto')
    expect(workspaceName('/Users/me/work/otto/')).toBe('otto')
    expect(sessionLabel('abcdef123456')).toBe('#abcdef12')
  })

  it('uses platform-specific send shortcut copy', () => {
    expect(sendHint(true)).toBe('Enter send · Shift+Enter newline · ⌘V paste image')
    expect(sendHint(false)).toBe('Enter send · Shift+Enter newline · Ctrl+V paste image')
  })
})
