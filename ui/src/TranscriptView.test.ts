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
