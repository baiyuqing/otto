import type { Compaction, Info, Message, Session, SessionListRow, Task, TurnSummary, WireEvent } from './types'
import { readSSE, type Frame } from './sse'

const TOKEN_KEY = 'otto.token'

// loadToken takes the token from the startup URL's query string, keeps it
// in sessionStorage (per tab, gone when the tab closes), and removes it from
// the address bar so it is not copied along with the URL.
export function loadToken(): string {
  const url = new URL(location.href)
  const fromURL = url.searchParams.get('token')
  if (fromURL) {
    sessionStorage.setItem(TOKEN_KEY, fromURL)
    url.searchParams.delete('token')
    history.replaceState(null, '', url.pathname + url.search + url.hash)
    return fromURL
  }
  return sessionStorage.getItem(TOKEN_KEY) ?? ''
}

let token = ''
export function setToken(t: string) {
  token = t
}

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message)
  }
}

async function request(path: string, init: RequestInit = {}): Promise<Response> {
  const headers = new Headers(init.headers)
  if (token) headers.set('Authorization', `Bearer ${token}`)
  if (init.body) headers.set('Content-Type', 'application/json')
  const res = await fetch(path, { ...init, headers })
  if (!res.ok) {
    let code = 'http_error'
    let message = `${res.status} ${res.statusText}`
    try {
      const body = await res.json()
      code = body.error?.code ?? code
      message = body.error?.message ?? message
    } catch {
      // non-JSON error body; keep the status text
    }
    throw new ApiError(res.status, code, message)
  }
  return res
}

const json = async <T>(path: string, init?: RequestInit): Promise<T> => (await request(path, init)).json()

export const api = {
  info: () => json<Info>('/v1/info'),
  listSessions: () => json<{ sessions: SessionListRow[] }>('/v1/sessions'),
  createSession: (resume?: string) =>
    json<Session>('/v1/sessions', { method: 'POST', body: JSON.stringify(resume ? { resume } : {}) }),
  getSession: (id: string) => json<Session>(`/v1/sessions/${id}`),
  history: (id: string) => json<Message[]>(`/v1/sessions/${id}/history`),
  getTurn: (id: string, turnId: string) => json<TurnSummary>(`/v1/sessions/${id}/turns/${turnId}`),
  cancelTurn: (id: string, turnId: string) => request(`/v1/sessions/${id}/turns/${turnId}/cancel`, { method: 'POST' }),
  listTasks: (id: string) => json<{ tasks: Task[] }>(`/v1/sessions/${id}/tasks`),
  cancelTask: (id: string, taskId: string) => request(`/v1/sessions/${id}/tasks/${taskId}/cancel`, { method: 'POST' }),
  compact: (id: string, focus: string, signal?: AbortSignal) =>
    json<Compaction>(`/v1/sessions/${id}/compact`, { method: 'POST', body: JSON.stringify({ focus }), signal }),

  // startTurn opens the turn's event stream from sequence 0.
  startTurn: (id: string, text: string) =>
    request(`/v1/sessions/${id}/turns`, { method: 'POST', body: JSON.stringify({ text, stream: true }) }),
  // attach re-reads a turn's events after sequence `after` (all of them when
  // omitted); used after a page reload or a dropped stream.
  attach: (id: string, turnId: string, after?: number) =>
    request(`/v1/sessions/${id}/turns/${turnId}/events${after === undefined ? '' : `?after=${after}`}`),
}

export interface TurnEvent {
  seq: number
  event: WireEvent
}

// events decodes an event-stream response into wire events with their
// sequence numbers.
export async function* events(res: Response): AsyncGenerator<TurnEvent> {
  if (!res.body) return
  for await (const frame of readSSE(res.body) as AsyncGenerator<Frame>) {
    yield { seq: frame.id ?? -1, event: JSON.parse(frame.data) as WireEvent }
  }
}
