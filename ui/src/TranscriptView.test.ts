// @vitest-environment jsdom
import { render, waitFor } from '@testing-library/react'
import { createElement } from 'react'
import { beforeEach, describe, expect, it, vi } from 'vitest'

const mermaidMock = vi.hoisted(() => ({
  render: vi.fn(async (_id: string, definition: string) => ({ svg: `<svg data-definition="${definition}"></svg>` })),
  initialize: vi.fn(),
}))

vi.mock('mermaid', () => ({
  default: mermaidMock,
}))

import { TranscriptView } from './TranscriptView'
import type { Item } from './wire'

describe('TranscriptView markdown extensions', () => {
  beforeEach(() => {
    mermaidMock.render.mockClear()
    mermaidMock.initialize.mockClear()
  })

  it('renders assistant math with KaTeX', async () => {
    const text = 'Inline math $E = mc^2$ and block math:\n\n$$\\int_0^1 x^2 dx = \\frac13$$'
    const { container } = render(createElement(TranscriptView, { activeSession: true, items: [{ kind: 'assistant', text }] }))

    await waitFor(() => expect(container.querySelector('.katex')).not.toBeNull())
    expect(container.textContent).toContain('E')
  })

  it('renders mermaid code fences as diagrams', async () => {
    const text = '```mermaid\ngraph TD\n  A[Start] --> B[Done]\n```'
    const { container } = render(createElement(TranscriptView, { activeSession: true, items: [{ kind: 'assistant', text }] }))

    await waitFor(() => expect(container.querySelector('.mermaid-diagram svg')).not.toBeNull())
    expect(mermaidMock.render).toHaveBeenCalledWith(expect.stringMatching(/^mermaid-/), 'graph TD\n  A[Start] --> B[Done]')
  })
})

describe('TranscriptView empty state', () => {
  it('empty session copy is a workspace, not a coding mascot', () => {
    const { container } = render(createElement(TranscriptView, { activeSession: true, items: [] }))
    expect(container.querySelector('.empty-orb')).toBeNull()
    expect(container.textContent).not.toMatch(/codebase/i)
    expect(container.textContent).not.toMatch(/Ready when you are/)
    expect(container.textContent).toMatch(/Session is open/)
  })
})

describe('TranscriptView item metadata', () => {
  it('renders request and response timestamps', () => {
    const items: Item[] = [
      { kind: 'user', text: 'hi', created_at: '2026-09-20T08:15:30Z' },
      { kind: 'assistant', text: 'hello', created_at: '2026-09-20T08:15:45Z' },
    ]
    const { container } = render(createElement(TranscriptView, { activeSession: true, items }))

    expect(container.querySelectorAll('time')).toHaveLength(2)
    expect(container.textContent).toContain('08:15:30')
    expect(container.textContent).toContain('08:15:45')
  })

  it('summarizes structured tool arguments instead of only showing the tool name', () => {
    const items: Item[] = [
      {
        kind: 'tool',
        id: 'call-1',
        name: 'edit',
        args: JSON.stringify({ path: 'src/app.rs', old_text: 'let old = true;', new_text: 'let new = true;' }),
        result: 'ok',
        isError: false,
      },
    ]
    const { container } = render(createElement(TranscriptView, { activeSession: true, items }))

    const summary = container.querySelector('summary')
    expect(summary?.textContent).toContain('edit src/app.rs')
    expect(summary?.textContent).toContain('- let old = true;')
    expect(summary?.textContent).toContain('+ let new = true;')
  })
})

describe('TranscriptView images', () => {
  it('renders a sent image from its stored data', () => {
    const image: Item = { kind: 'image', data: 'iVBORw0KGgo=', mime_type: 'image/png' }
    const { container } = render(createElement(TranscriptView, { activeSession: true, items: [image] }))

    const element = container.querySelector('img')
    expect(element?.getAttribute('src')).toBe('data:image/png;base64,iVBORw0KGgo=')
    expect(element?.getAttribute('alt')).toBe('Sent image')
  })
})

describe('TranscriptView reasoning', () => {
  it('shows reasoning collapsed with its first line as the summary', () => {
    const items: Item[] = [
      { kind: 'reasoning', text: 'Weigh options\nthen pick the smaller diff' },
      { kind: 'assistant', text: 'ok' },
    ]
    const { container } = render(createElement(TranscriptView, { activeSession: true, items }))

    const details = container.querySelector('details.item.reasoning') as HTMLDetailsElement
    expect(details).not.toBeNull()
    expect(details.open).toBe(false)
    expect(details.querySelector('summary')?.textContent).toContain('Weigh options')
    expect(details.textContent).toContain('then pick the smaller diff')
  })
})
