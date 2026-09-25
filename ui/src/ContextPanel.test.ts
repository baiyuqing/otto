// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { createElement } from 'react'
import { afterEach, describe, expect, it, vi } from 'vitest'

const api = vi.hoisted(() => ({ context: vi.fn() }))
vi.mock('./api', () => ({ api }))

import { ContextPanel } from './ContextPanel'

const report = {
  model: 'gpt-5',
  context_window: 272000,
  compaction_threshold: 200000,
  estimated_total: 41200,
  reported_input_tokens: 39800,
  sections: [
    {
      kind: 'system_prompt',
      tokens: 6100,
      items: [
        { label: 'Base', tokens: 1200, text: 'You are Otto.' },
        { label: 'Workspace instructions', tokens: 4900, text: '# house rules' },
      ],
    },
    { kind: 'messages', tokens: 35100, items: [{ label: '#1 user', tokens: 35100, text: 'hello' }] },
  ],
}

describe('ContextPanel', () => {
  afterEach(() => {
    cleanup()
    api.context.mockReset()
  })

  it('renders the header, one bar per section, and each item text', async () => {
    api.context.mockResolvedValue(report)
    render(createElement(ContextPanel, { sessionId: 's1', refreshKey: 0, onClose: () => {}, onError: () => {} }))

    await screen.findByText('System prompt')
    expect(api.context).toHaveBeenCalledWith('s1')
    expect(screen.getByText(/gpt-5 · ~41,200 \/ 272,000 tokens \(estimate\)/)).toBeTruthy()
    expect(screen.getByText(/last reported 39,800/)).toBeTruthy()
    expect(screen.getByText(/compacts at 200,000/)).toBeTruthy()
    expect(screen.getByText('Messages (1)')).toBeTruthy()
    expect(screen.getAllByRole('meter')).toHaveLength(2)
    expect(screen.getByText('# house rules').tagName).toBe('PRE')
  })

  it('re-reads the report when refreshKey changes', async () => {
    api.context.mockResolvedValue(report)
    const props = { sessionId: 's1', onClose: () => {}, onError: () => {} }
    const { rerender } = render(createElement(ContextPanel, { ...props, refreshKey: 0 }))
    await waitFor(() => expect(api.context).toHaveBeenCalledTimes(1))
    rerender(createElement(ContextPanel, { ...props, refreshKey: 1 }))
    await waitFor(() => expect(api.context).toHaveBeenCalledTimes(2))
  })

  it('closes from its close button', async () => {
    api.context.mockResolvedValue(report)
    const onClose = vi.fn()
    render(createElement(ContextPanel, { sessionId: 's1', refreshKey: 0, onClose, onError: () => {} }))
    fireEvent.click(await screen.findByRole('button', { name: 'Close context' }))
    expect(onClose).toHaveBeenCalled()
  })
})
