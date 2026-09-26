import { useEffect, useRef, useState } from 'react'
import { ApiError, streamStatus, type SessionStatus } from './api'

export const STATUS_RECONNECT_MS = 1000

// useStatus keeps the latest GET /v1/status snapshot as a map by session id.
// It reconnects after STATUS_RECONNECT_MS whenever the stream ends or
// errors, for as long as the component stays mounted, and calls
// onUnknownSession once per snapshot that names a session id outside
// knownIds (the caller's job is deciding what "unknown" means and reacting,
// e.g. by refreshing its session list). A 401 (the server issued a new
// token) is reported through onError instead, and stops reconnecting.
export function useStatus(
  knownIds: Set<string>,
  onUnknownSession: () => void,
  onError: (error: unknown) => void,
): Map<string, SessionStatus> {
  const [status, setStatus] = useState<Map<string, SessionStatus>>(new Map())
  const knownIdsRef = useRef(knownIds)
  knownIdsRef.current = knownIds
  const onUnknownRef = useRef(onUnknownSession)
  onUnknownRef.current = onUnknownSession
  const onErrorRef = useRef(onError)
  onErrorRef.current = onError

  useEffect(() => {
    let stopped = false
    const controller = new AbortController()

    const run = async () => {
      while (!stopped) {
        try {
          for await (const snapshot of streamStatus(controller.signal)) {
            if (stopped) return
            setStatus(new Map(snapshot.sessions.map((s) => [s.id, s])))
            if (snapshot.sessions.some((s) => !knownIdsRef.current.has(s.id))) onUnknownRef.current()
          }
        } catch (e) {
          if (e instanceof ApiError && e.status === 401) {
            onErrorRef.current(e)
            return
          }
          // Connection dropped or aborted; fall through to reconnect below.
        }
        if (stopped) return
        await new Promise((resolve) => setTimeout(resolve, STATUS_RECONNECT_MS))
      }
    }
    void run()

    return () => {
      stopped = true
      controller.abort()
    }
  }, [])

  return status
}
