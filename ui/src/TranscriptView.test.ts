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

describe('TranscriptView images', () => {
  it('renders a sent image from its stored data', () => {
    const image: Item = { kind: 'image', data: 'iVBORw0KGgo=', mime_type: 'image/png' }
    const { container } = render(createElement(TranscriptView, { activeSession: true, items: [image] }))

    const element = container.querySelector('img')
    expect(element?.getAttribute('src')).toBe('data:image/png;base64,iVBORw0KGgo=')
    expect(element?.getAttribute('alt')).toBe('Sent image')
  })
})
