import { describe, expect, it } from 'vitest'
import { mcpServerLine, sendHint, sessionLabel, workspaceName } from './uiText'

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

  it('formats one line per MCP server state', () => {
    expect(mcpServerLine({ name: 'docs', transport: 'http', protocol_version: '2026-07-28', state: 'connected', tools: 3, error: null })).toBe(
      'docs: connected (3 tools) (http, 2026-07-28)',
    )
    expect(mcpServerLine({ name: 'shell', transport: 'stdio', protocol_version: null, state: 'disabled', tools: 0, error: null })).toBe(
      'shell: disabled (stdio, -)',
    )
    expect(
      mcpServerLine({ name: 'legacy-tool', transport: 'http', protocol_version: '2025-11-25', state: 'needs_login', tools: 0, error: null }),
    ).toBe('legacy-tool: needs_login (http, 2025-11-25) - run: otto mcp login legacy-tool')
    expect(mcpServerLine({ name: 'broken', transport: 'stdio', protocol_version: null, state: 'failed', tools: 0, error: 'spawn failed: not found' })).toBe(
      'broken: failed: spawn failed: not found (stdio, -)',
    )
  })
})
