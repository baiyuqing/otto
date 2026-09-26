import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'

const css = readFileSync(join(dirname(fileURLToPath(import.meta.url)), 'app.css'), 'utf8')

describe('web UI theme', () => {
  it('gives code blocks their own readable foreground and background', () => {
    expect(css).toMatch(/--code-bg:/)
    expect(css).toMatch(/--code-fg:/)
    expect(css).toMatch(/\.item\.assistant pre \{[^}]*background:\s*var\(--code-bg\)/s)
    expect(css).toMatch(/\.item\.assistant pre \{[^}]*color:\s*var\(--code-fg\)/s)
    expect(css).not.toMatch(/#020617/)
  })

  it('does not use the previous pastel sky-and-purple look', () => {
    expect(css).not.toMatch(/#a855f7/)
    expect(css).not.toMatch(/#eef5ff/)
    expect(css).not.toMatch(/#38bdf8/)
    expect(css).not.toMatch(/\bInter\b/)
    expect(css).not.toMatch(/border-radius:\s*999px/)
  })

  it('hides the sidebar below 720px behind the topbar toggle', () => {
    const narrow = css.match(/@media \(max-width: 720px\) \{[\s\S]*?\n\}/)?.[0] ?? ''
    expect(narrow).toMatch(/\.sidebar-toggle \{ display: inline-flex; \}/)
    expect(narrow).toMatch(/\.sidebar-wrap \{ display: none; \}/)
  })
})
