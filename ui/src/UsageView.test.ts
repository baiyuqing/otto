// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { createElement } from 'react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const api = vi.hoisted(() => ({ usageDaily: vi.fn() }))
const mermaid = vi.hoisted(() => ({
  initialize: vi.fn(),
  render: vi.fn(async (_id: string, _code: string) => ({ svg: '<svg aria-label="chart"></svg>' })),
}))

vi.mock('./api', () => ({ api }))
vi.mock('mermaid', () => ({ default: mermaid }))

import { UsageView } from './UsageView'

const analysis = {
  summary: {
    requests: 2,
    reported_requests: 2,
    input_tokens: 30,
    output_tokens: 5,
    cached_input_tokens: 10,
    cache_hit_rate: 1 / 3,
  },
  daily: [
    { date: '2026-09-17', requests: 0, reported_requests: 0, input_tokens: 0, output_tokens: 0, cached_input_tokens: 0 },
    { date: '2026-09-18', requests: 1, reported_requests: 1, input_tokens: 10, output_tokens: 2, cached_input_tokens: 4 },
    { date: '2026-09-19', requests: 1, reported_requests: 1, input_tokens: 20, output_tokens: 3, cached_input_tokens: 6 },
  ],
}

describe('UsageView', () => {
  beforeEach(() => {
    api.usageDaily.mockResolvedValue(analysis)
    mermaid.render.mockClear()
  })

  afterEach(() => {
    cleanup()
    vi.clearAllMocks()
  })

  it('renders totals and a Mermaid daily token chart', async () => {
    render(createElement(UsageView, { onError: vi.fn() }))

    expect(await screen.findByText('30')).toBeTruthy()
    expect(screen.getByText('33.3%')).toBeTruthy()
    expect(screen.getByRole('group', { name: 'Usage totals' })).toBeTruthy()
    await waitFor(() => expect(mermaid.render).toHaveBeenCalled())
    const chart = mermaid.render.mock.calls[0][1]
    expect(chart).toContain('xychart')
    expect(chart).toContain('line [0, 10, 20]')
    expect(chart).toContain('line [0, 2, 3]')
    expect(chart).toContain('line [0, 4, 6]')
  })

  it('reloads when the range changes', async () => {
    render(createElement(UsageView, { onError: vi.fn() }))
    await waitFor(() => expect(api.usageDaily).toHaveBeenCalledWith(30))

    fireEvent.click(screen.getByRole('button', { name: '7 days' }))
    await waitFor(() => expect(api.usageDaily).toHaveBeenCalledWith(7))
  })
})
