// @vitest-environment jsdom
import { act, cleanup, render, screen } from '@testing-library/react'
import { createElement } from 'react'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'

const api = vi.hoisted(() => ({ scheduleClaim: vi.fn(), scheduleDelivered: vi.fn() }))
vi.mock('./api', async (importOriginal) => ({ ...(await importOriginal<typeof import('./api')>()), api }))
vi.mock('./TranscriptView', () => ({ MarkdownView: ({ text }: { text: string }) => createElement('p', null, text) }))
import { ScheduledNotices } from './ScheduledNotices'
import { SCHEDULE_FIRST_POLL_MS } from './schedule'

beforeEach(() => vi.useFakeTimers())
afterEach(() => {
  cleanup()
  vi.useRealTimers()
  vi.clearAllMocks()
})

it('renders a claimed item in a status region and reports it delivered', async () => {
  api.scheduleClaim.mockResolvedValueOnce({ items: [{ id: 's7', due_at: 42, text: 'stand up' }] }).mockResolvedValue({ items: [] })
  api.scheduleDelivered.mockResolvedValue({})
  render(createElement(ScheduledNotices))
  await act(async () => {
    await vi.advanceTimersByTimeAsync(SCHEDULE_FIRST_POLL_MS)
  })
  expect(screen.getByRole('status').textContent).toContain('stand up')
  expect(api.scheduleDelivered).toHaveBeenCalledWith('s7', 42)
})
