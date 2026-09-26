// @vitest-environment jsdom
import { act, renderHook } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { StatusSnapshot } from './api'

const api = vi.hoisted(() => ({ streamStatus: vi.fn() }))
vi.mock('./api', () => ({ streamStatus: api.streamStatus }))

import { useStatus } from './status'

const snapshot = (ids: string[]): StatusSnapshot => ({
  sessions: ids.map((id) => ({ id, workspace: '/w', turn: 'running', approvals: 0, tasks: 0 })),
})

async function* oneShot(values: StatusSnapshot[]) {
  for (const v of values) yield v
}

describe('useStatus', () => {
  beforeEach(() => {
    vi.useFakeTimers()
  })

  afterEach(() => {
    vi.useRealTimers()
    vi.clearAllMocks()
  })

  it('calls onUnknownSession when a snapshot names an id outside knownIds, and reconnects after the stream ends', async () => {
    const onUnknown = vi.fn()
    api.streamStatus.mockReturnValueOnce(oneShot([snapshot(['new-id'])]))
    api.streamStatus.mockReturnValueOnce(oneShot([]))

    const { result } = renderHook(() => useStatus(new Set(['known']), onUnknown))

    await act(async () => {
      await Promise.resolve()
      await Promise.resolve()
    })

    expect(onUnknown).toHaveBeenCalledTimes(1)
    expect(result.current.get('new-id')?.turn).toBe('running')
    expect(api.streamStatus).toHaveBeenCalledTimes(1)

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1000)
    })

    expect(api.streamStatus).toHaveBeenCalledTimes(2)
  })

  it('does not call onUnknownSession when every id is already known', async () => {
    const onUnknown = vi.fn()
    api.streamStatus.mockReturnValueOnce(oneShot([snapshot(['known'])]))

    renderHook(() => useStatus(new Set(['known']), onUnknown))

    await act(async () => {
      await Promise.resolve()
      await Promise.resolve()
    })

    expect(onUnknown).not.toHaveBeenCalled()
  })
})
