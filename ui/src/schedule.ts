import { ApiError, type ScheduleItem } from './api'

// How often an open page asks for due scheduled deliveries.
export const SCHEDULE_POLL_MS = 15000
// The first claim runs shortly after load rather than immediately.
export const SCHEDULE_FIRST_POLL_MS = 1000

export interface ScheduleDeps {
  claim: () => Promise<{ items: ScheduleItem[] }>
  delivered: (id: string, dueAt: number) => Promise<unknown>
  show: (item: ScheduleItem) => void
  /** Called once per streak of failed polls, not on every failure. */
  onError?: (err: unknown) => void
}

/**
 * Polls claim until the returned function is called. A 404 from claim stops
 * polling for good (server without schedule routes). Each claimed item is
 * shown first, then reported delivered with its unchanged due_at; if that
 * report fails the notice stays and the server offers the item again after
 * its 2 minute claim expires.
 */
export function startSchedulePolling(deps: ScheduleDeps): () => void {
  let stopped = false
  let inFlight = false
  let failing = false
  const timers: ReturnType<typeof setTimeout | typeof setInterval>[] = []
  const stop = () => {
    stopped = true
    timers.forEach((t) => {
      clearTimeout(t)
      clearInterval(t)
    })
  }
  const poll = async () => {
    if (inFlight || stopped) return
    inFlight = true
    try {
      const { items } = await deps.claim()
      if (stopped) return
      let clean = true
      for (const item of items) {
        deps.show(item)
        try {
          await deps.delivered(item.id, item.due_at)
        } catch (err) {
          clean = false
          reportOnce(err)
        }
      }
      if (clean) failing = false
    } catch (err) {
      if (err instanceof ApiError && err.status === 404) {
        stop()
        return
      }
      reportOnce(err)
    } finally {
      inFlight = false
    }
  }
  const reportOnce = (err: unknown) => {
    if (failing) return
    failing = true
    deps.onError?.(err)
  }
  timers.push(setTimeout(() => void poll(), SCHEDULE_FIRST_POLL_MS))
  timers.push(setInterval(() => void poll(), SCHEDULE_POLL_MS))
  return stop
}
