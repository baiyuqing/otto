import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { ApiError, type ScheduleItem } from './api'
import { SCHEDULE_FIRST_POLL_MS, SCHEDULE_POLL_MS, startSchedulePolling } from './schedule'

const item: ScheduleItem = { id: 's7', due_at: 1759622400000, text: '⏰ stand up' }

beforeEach(() => vi.useFakeTimers())
afterEach(() => vi.useRealTimers())

describe('startSchedulePolling', () => {
  it('shows a claimed item, then reports delivered with the same due_at', async () => {
    const calls: string[] = []
    const claim = vi.fn().mockResolvedValue({ items: [item] })
    const delivered = vi.fn(async () => void calls.push('delivered'))
    const show = vi.fn(() => void calls.push('show'))
    const stop = startSchedulePolling({ claim, delivered, show })
    await vi.advanceTimersByTimeAsync(SCHEDULE_FIRST_POLL_MS)
    expect(show).toHaveBeenCalledWith(item)
    expect(delivered).toHaveBeenCalledWith('s7', 1759622400000)
    expect(calls).toEqual(['show', 'delivered'])
    stop()
  })

  it('keeps the notice and reports once per streak when delivered fails', async () => {
    const claim = vi.fn().mockResolvedValue({ items: [item] })
    const delivered = vi.fn().mockRejectedValue(new ApiError(500, 'x', 'boom'))
    const show = vi.fn()
    const onError = vi.fn()
    const stop = startSchedulePolling({ claim, delivered, show, onError })
    await vi.advanceTimersByTimeAsync(SCHEDULE_FIRST_POLL_MS)
    await vi.advanceTimersByTimeAsync(SCHEDULE_POLL_MS)
    expect(show).toHaveBeenCalledTimes(2)
    expect(onError).toHaveBeenCalledTimes(1)
    stop()
  })

  it('stops polling on 404', async () => {
    const claim = vi.fn().mockRejectedValue(new ApiError(404, 'not_found', 'no'))
    startSchedulePolling({ claim, delivered: vi.fn(), show: vi.fn() })
    await vi.advanceTimersByTimeAsync(SCHEDULE_FIRST_POLL_MS + 3 * SCHEDULE_POLL_MS)
    expect(claim).toHaveBeenCalledTimes(1)
  })

  it('keeps polling after other errors and stops when stopped', async () => {
    const claim = vi.fn().mockRejectedValue(new ApiError(500, 'x', 'boom'))
    const onError = vi.fn()
    const stop = startSchedulePolling({ claim, delivered: vi.fn(), show: vi.fn(), onError })
    await vi.advanceTimersByTimeAsync(SCHEDULE_FIRST_POLL_MS + SCHEDULE_POLL_MS)
    expect(claim).toHaveBeenCalledTimes(2)
    expect(onError).toHaveBeenCalledTimes(1)
    stop()
    await vi.advanceTimersByTimeAsync(3 * SCHEDULE_POLL_MS)
    expect(claim).toHaveBeenCalledTimes(2)
  })
})
