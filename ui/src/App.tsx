import { useCallback, useEffect, useState } from 'react'
import { api, ApiError, events, loadToken, setToken } from './api'
import { fromHistory, reduce, type Item } from './transcript'
import type { Session, SessionListRow } from './types'
import { SessionPicker } from './SessionPicker'
import { TranscriptView } from './TranscriptView'
import { Composer } from './Composer'

setToken(loadToken())

const describe = (e: unknown) => (e instanceof Error ? e.message : String(e))

export function App() {
  const [sessions, setSessions] = useState<SessionListRow[]>([])
  const [session, setSession] = useState<Session | null>(null)
  const [items, setItems] = useState<Item[]>([])
  const [turnId, setTurnId] = useState<string | null>(null)
  const [error, setError] = useState('')

  const refreshSessions = useCallback(
    () =>
      api
        .listSessions()
        .then((r) => setSessions(r.sessions))
        .catch((e) => setError(describe(e))),
    [],
  )

  // consume reads a turn's stream to the end. The stream closes when the
  // turn is done, but also when the connection drops, so it then asks the
  // server for the session's turn state and re-attaches after the last
  // sequence number it saw.
  const consume = useCallback(async (sessionId: string, res: Response) => {
    let last = -1
    try {
      for (;;) {
        for await (const { seq, event } of events(res)) {
          last = seq
          setItems((prev) => reduce(prev, event))
        }
        const s = await api.getSession(sessionId)
        setSession(s)
        if (!s.turn || s.turn.status !== 'running') {
          if (s.turn?.status === 'canceled') setItems((prev) => [...prev, { kind: 'notice', text: 'Turn canceled' }])
          return
        }
        res = await api.attach(sessionId, s.turn.id, last >= 0 ? last : undefined)
      }
    } catch (e) {
      setError(describe(e))
    } finally {
      setTurnId(null)
    }
  }, [])

  const open = useCallback(
    async (id?: string) => {
      setError('')
      try {
        const s = await api.createSession(id)
        location.hash = s.id
        setSession(s)
        setItems(fromHistory(await api.history(s.id)))
        void refreshSessions()
        if (s.turn?.status === 'running') {
          setTurnId(s.turn.id)
          void consume(s.id, await api.attach(s.id, s.turn.id))
        }
      } catch (e) {
        setError(describe(e))
      }
    },
    [consume, refreshSessions],
  )

  useEffect(() => {
    void refreshSessions()
    const id = location.hash.slice(1)
    if (id) void open(id)
    // Runs once on mount; main.tsx does not use StrictMode, so this does not
    // attach twice in development.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const send = async (text: string) => {
    if (!session) return
    setError('')
    try {
      const res = await api.startTurn(session.id, text)
      // The stream carries no turn id; the session does.
      const s = await api.getSession(session.id)
      setSession(s)
      setTurnId(s.turn?.id ?? null)
      setItems((prev) => [...prev, { kind: 'user', text }])
      await consume(session.id, res)
    } catch (e) {
      setError(describe(e))
      if (e instanceof ApiError && e.status === 409) {
        // Another client started a turn; follow it.
        const s = await api.getSession(session.id).catch(() => null)
        if (s?.turn?.status === 'running') {
          setSession(s)
          setTurnId(s.turn.id)
          void consume(s.id, await api.attach(s.id, s.turn.id))
        }
      }
    }
  }

  const cancel = () => {
    if (session && turnId) api.cancelTurn(session.id, turnId).catch((e) => setError(describe(e)))
  }

  return (
    <div className="app">
      <div className="topbar">
        <SessionPicker sessions={sessions} current={session?.id ?? ''} disabled={turnId !== null} onOpen={open} />
        <span className="spacer" />
        {session && (
          <span className="meta">
            {session.provider} · {session.model} · {session.workspace}
          </span>
        )}
      </div>
      {error && <div className="banner">{error}</div>}
      <TranscriptView items={items} />
      <Composer disabled={!session} running={turnId !== null} onSend={send} onCancel={cancel} />
    </div>
  )
}
