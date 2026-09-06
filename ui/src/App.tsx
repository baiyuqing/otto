import { useCallback, useEffect, useRef, useState } from 'react'
import { api, ApiError, events, loadToken, setToken } from './api'
import { fromHistory, reduce, type Item } from './transcript'
import type { Info, Session, SessionListRow, Usage } from './types'
import { SessionPicker } from './SessionPicker'
import { TranscriptView } from './TranscriptView'
import { Composer } from './Composer'
import { Footer } from './Footer'
import { Tasks } from './Tasks'
import { sessionLabel, workspaceName } from './uiText'

setToken(loadToken())

const describe = (e: unknown) => (e instanceof Error ? e.message : String(e))

export function App() {
  const [info, setInfo] = useState<Info | null>(null)
  const [sessions, setSessions] = useState<SessionListRow[]>([])
  const [session, setSession] = useState<Session | null>(null)
  const [items, setItems] = useState<Item[]>([])
  const [turnId, setTurnId] = useState<string | null>(null)
  const [turnUsage, setTurnUsage] = useState<Usage | null>(null)
  const [compacting, setCompacting] = useState(false)
  const [tasksKey, setTasksKey] = useState(0)
  const [error, setError] = useState('')
  const compactAbort = useRef<AbortController | null>(null)

  const fail = useCallback((e: unknown) => setError(describe(e)), [])

  const refreshSessions = useCallback(
    () =>
      api
        .listSessions()
        .then((r) => setSessions(r.sessions))
        .catch(fail),
    [fail],
  )

  // consume reads a turn's stream to the end. The stream closes when the
  // turn is done, but also when the connection drops, so it then asks the
  // server for the session's turn state and re-attaches after the last
  // sequence number it saw.
  const consume = useCallback(
    async (sessionId: string, res: Response) => {
      let last = -1
      try {
        for (;;) {
          for await (const { seq, event } of events(res)) {
            last = seq
            if (event.type === 'provider_usage' && event.usage) {
              const u = event.usage
              setTurnUsage((prev) => ({
                input_tokens: (prev?.input_tokens ?? 0) + u.input_tokens,
                output_tokens: (prev?.output_tokens ?? 0) + u.output_tokens,
                cached_input_tokens: (prev?.cached_input_tokens ?? 0) + (u.cached_input_tokens ?? 0),
              }))
            }
            if (event.type === 'notification') setTasksKey((k) => k + 1)
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
        fail(e)
      } finally {
        setTurnId(null)
        setTurnUsage(null)
        setTasksKey((k) => k + 1)
      }
    },
    [fail],
  )

  const open = useCallback(
    async (id?: string) => {
      setError('')
      compactAbort.current?.abort()
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
        fail(e)
      }
    },
    [consume, refreshSessions, fail],
  )

  useEffect(() => {
    api.info().then(setInfo).catch(fail)
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
      fail(e)
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
    if (session && turnId) api.cancelTurn(session.id, turnId).catch(fail)
  }

  const compact = async (focus: string) => {
    if (!session) return
    setError('')
    setCompacting(true)
    const ac = new AbortController()
    compactAbort.current = ac
    try {
      const c = await api.compact(session.id, focus, ac.signal)
      const text = c.noop
        ? 'Nothing to compact'
        : `Context compacted: ${c.tokens_before} → ~${c.estimated_tokens_after} tokens (${c.reason})`
      setItems((prev) => [...prev, { kind: 'notice', text }])
      setSession(await api.getSession(session.id))
    } catch (e) {
      if (!ac.signal.aborted) fail(e)
    } finally {
      if (compactAbort.current === ac) compactAbort.current = null
      setCompacting(false)
    }
  }

  const renameSession = async () => {
    if (!session) return
    const current = session.name ?? ''
    const name = window.prompt('Rename session', current)?.trim()
    if (!name) return
    setError('')
    try {
      const renamed = await api.renameSession(session.id, name)
      setSession(renamed)
      void refreshSessions()
    } catch (e) {
      fail(e)
    }
  }

  const busy = turnId !== null || compacting

  return (
    <div className="app">
      <header className="topbar">
        <div className="brand" aria-label="Otto Web UI">
          <span className="brand-mark">O</span>
          <div>
            <div className="brand-name">Otto</div>
            <div className="brand-subtitle">AI coding agent</div>
          </div>
        </div>
        <SessionPicker sessions={sessions} current={session?.id ?? ''} disabled={busy} onOpen={open} />
        <span className="spacer" />
        {session && (
          <div className="session-chip" title={session.id}>
            <span>{session.name ?? sessionLabel(session.id)}</span>
            <strong>{workspaceName(session.workspace)}</strong>
            <button type="button" disabled={busy} onClick={renameSession}>
              Rename
            </button>
          </div>
        )}
      </header>
      {error && (
        <div className="banner" role="alert">
          <span>{error}</span>
          <button aria-label="Dismiss error" onClick={() => setError('')}>
            ×
          </button>
        </div>
      )}
      <main className="workspace-shell">
        <TranscriptView items={items} activeSession={session !== null} />
      </main>
      {session && <Tasks sessionId={session.id} refreshKey={tasksKey} onError={fail} />}
      <Composer
        disabled={!session}
        running={turnId !== null}
        compacting={compacting}
        onSend={send}
        onCancel={cancel}
        onCompact={compact}
      />
      <Footer info={info} session={session} turnUsage={turnUsage} />
    </div>
  )
}
