import { useCallback, useEffect, useRef, useState } from 'react'
import { api, events, loadToken, setToken, type UsageSummary } from './api'
import { parseWebCommand, supportedCommands } from './commands'
import { fromHistory, phase, reduce, statusLine, type Info, type Item, type Session, type SessionListRow, type Usage } from './wire'
import { Sidebar } from './Sidebar'
import { TranscriptView } from './TranscriptView'
import { Composer } from './Composer'
import { Footer } from './Footer'
import { ContextPanel } from './ContextPanel'
import { Tasks } from './Tasks'
import { UsageView } from './UsageView'
import { WorkflowsView } from './WorkflowsView'
import { ChangesView } from './ChangesView'
import { AgentsView } from './AgentsView'
import { generatedSkillsText, mcpServerLine, sessionLabel, workspaceName } from './uiText'
import { IDLE_POLL_MS, NOTICE_POLL_MS, idleFollow } from './follow'
import { useStatus } from './status'
import logo from '../logo.svg'

setToken(loadToken())

const describe = (e: unknown) => (e instanceof Error ? e.message : String(e))

const helpText = () => supportedCommands.map((command) => `${command.name.padEnd(10)} ${command.description}`).join('\n')

const usageText = (usage: Usage) =>
  `${usage.input_tokens} input / ${usage.output_tokens} output` +
  (usage.cached_input_tokens ? ` / ${usage.cached_input_tokens} cached` : '')

const sessionText = (s: Session) =>
  [
    `Session: ${s.name ?? sessionLabel(s.id)} (${s.id})`,
    `Workspace: ${s.workspace}`,
    `Profile: ${s.profile}`,
    `Provider: ${s.provider}`,
    `Model: ${s.model}`,
    `Thinking: ${s.thinking || 'default'}`,
    `Sandbox: ${s.sandbox.summary}`,
    `Context: ${s.context_input_tokens} / ${s.context_window} tokens`,
    `Usage: ${usageText(s.usage)}`,
    `Turn: ${s.turn ? `${s.turn.status} ${s.turn.id}` : 'none'}`,
  ].join('\n')

const taskText = (task: {
  id: string
  name?: string
  agent: string
  description: string
  model?: string
  status: string
  steps: number
  tool_calls: number
  last_tool?: string
  last_text?: string
  result?: string
  error?: string
}) =>
  [
    `Task: ${task.name ?? task.id} (${task.id})`,
    `Agent: ${task.agent}`,
    `Description: ${task.description}`,
    task.model ? `Model: ${task.model}` : '',
    `Status: ${task.status}`,
    `Steps: ${task.steps}, tool calls: ${task.tool_calls}`,
    task.last_tool ? `Last tool: ${task.last_tool}` : '',
    task.last_text ? `Last text: ${task.last_text}` : '',
    task.error ? `Error: ${task.error}` : '',
    task.result ? `Result:\n${task.result}` : '',
  ]
    .filter(Boolean)
    .join('\n')

export function App() {
  const [view, setView] = useState<'chat' | 'workflows' | 'usage' | 'agents' | 'changes'>('chat')
  const [changesWorkspace, setChangesWorkspace] = useState('')
  const [info, setInfo] = useState<Info | null>(null)
  const [sessions, setSessions] = useState<SessionListRow[]>([])
  const [session, setSession] = useState<Session | null>(null)
  const [items, setItems] = useState<Item[]>([])
  const [turnId, setTurnId] = useState<string | null>(null)
  const [turnUsage, setTurnUsage] = useState<Usage | null>(null)
  const [recordedUsage, setRecordedUsage] = useState<UsageSummary | null>(null)
  const [compacting, setCompacting] = useState(false)
  const [reflecting, setReflecting] = useState(false)
  const [queuedInput, setQueuedInput] = useState('')
  const [queuedTurn, setQueuedTurn] = useState(false)
  const [tasksKey, setTasksKey] = useState(0)
  const [showContext, setShowContext] = useState(false)
  const [renameDraft, setRenameDraft] = useState<string | null>(null)
  const [renaming, setRenaming] = useState(false)
  const [error, setError] = useState('')
  const [sidebarOpen, setSidebarOpen] = useState(false)
  const compactAbort = useRef<AbortController | null>(null)
  const reflectAbort = useRef<AbortController | null>(null)
  // The running turn's phase and when it and the turn started (ms). A turn
  // re-attached after a reload counts from the attach, not the server start.
  const [phaseState, setPhaseState] = useState<{ name: string; since: number; turnStart: number; retryEvent?: string } | null>(null)
  const [now, setNow] = useState(Date.now())
  const approvalMessageRef = useRef<{ sessionId: string; turnId: string; text: string } | null>(null)

  const fail = useCallback((e: unknown) => setError(describe(e)), [])

  const refreshSessions = useCallback(
    () =>
      api
        .listSessions()
        .then((r) => setSessions(r.sessions))
        .catch(fail),
    [fail],
  )

  const refreshUsage = useCallback(() => api.usage().then(setRecordedUsage).catch(fail), [fail])

  // The status stream reports every open session in this process, including
  // ones opened elsewhere; a snapshot naming a session outside the current
  // list means the session list is stale.
  const knownSessionIds = new Set(sessions.map((s) => s.id))
  const status = useStatus(knownSessionIds, () => void refreshSessions(), fail)

  // consume reads a turn's stream to the end. The stream closes when the
  // turn is done, but also when the connection drops, so it then asks the
  // server for the session's turn state and re-attaches after the last
  // sequence number it saw.
  const consume = useCallback(
    async (sessionId: string, res: Response) => {
      let last = -1
      try {
        for (;;) {
          for await (const { seq, event, raw } of events(res)) {
            last = seq
            setQueuedTurn(false)
            if (event.type === 'provider_usage' && event.usage) {
              const u = event.usage
              setTurnUsage((prev) => ({
                input_tokens: (prev?.input_tokens ?? 0) + u.input_tokens,
                output_tokens: (prev?.output_tokens ?? 0) + u.output_tokens,
                cached_input_tokens: (prev?.cached_input_tokens ?? 0) + (u.cached_input_tokens ?? 0),
              }))
            }
            if (event.type === 'notification') setTasksKey((k) => k + 1)
            const next = phase(raw)
            if (next) setPhaseState((p) => (p && p.name !== next ? { ...p, name: next, since: Date.now(), retryEvent: event.type === 'provider_retry' ? raw : undefined } : p))
            setItems((prev) => reduce(prev, raw))
          }
          const s = await api.getSession(sessionId)
          setSession(s)
          if (!s.turn || s.turn.status !== 'running') {
            if (s.turn?.status === 'canceled') setItems((prev) => [...prev, { kind: 'notice', text: 'Turn canceled' }])
            const queued = queuedInputRef.current.trim()
            const pendingMessage = approvalMessageRef.current
            const awaitingControlReply = pendingMessage !== null && pendingMessage.sessionId === sessionId && pendingMessage.text === queued
            const shouldSendQueued = s.turn?.status === 'ok' && queued && !awaitingControlReply
            if (shouldSendQueued) {
              queuedInputRef.current = ''
              setQueuedInput('')
              turnIdRef.current = null
              setTurnId(null)
            }
            if (shouldSendQueued) void sendRef.current?.(queued)
            return
          }
          res = await api.attach(sessionId, s.turn.id, last >= 0 ? last : undefined)
        }
      } catch (e) {
        fail(e)
      } finally {
        setQueuedTurn(false)
        setTurnId(null)
        setTurnUsage(null)
        setTasksKey((k) => k + 1)
        void refreshUsage()
      }
    },
    [fail, refreshUsage],
  )

  const open = useCallback(
    async (id?: string, workspace?: string) => {
      setError('')
      compactAbort.current?.abort()
      reflectAbort.current?.abort()
      try {
        const s = await api.createSession(id, workspace)
        location.hash = s.id
        queuedInputRef.current = ''
        setQueuedInput('')
        setSession(s)
        const running = s.turn?.status === 'running' ? s.turn.id : undefined
        setItems(fromHistory(await api.history(s.id, running)))
        void refreshSessions()
        if (running) {
          setTurnId(running)
          void consume(s.id, await api.attach(s.id, running))
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
    void refreshUsage()
    const id = location.hash.slice(1)
    if (id) void open(id)
    // Runs once on mount; main.tsx does not use StrictMode, so this does not
    // attach twice in development.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  useEffect(() => {
    if (turnId === null) {
      setPhaseState(null)
      return
    }
    const start = Date.now()
    setNow(start)
    setPhaseState({ name: 'waiting for model', since: start, turnStart: start })
    const timer = setInterval(() => setNow(Date.now()), 1000)
    return () => clearInterval(timer)
  }, [turnId])

  const sessionRef = useRef(session)
  sessionRef.current = session
  const turnIdRef = useRef(turnId)
  turnIdRef.current = turnId
  const queuedInputRef = useRef(queuedInput)
  queuedInputRef.current = queuedInput
  const sendRef = useRef<((text: string, image?: { data: string; mime_type: string }) => Promise<void>) | null>(null)
  // A compaction and a reflection both hold the session: the server refuses
  // turns while either runs, so neither is polled over or sent into.
  const holding = compacting || reflecting
  const compactingRef = useRef(holding)
  compactingRef.current = holding

  // Server-started wakes (remind) never go through startTurn,
  // so an idle page has to poll the open session and attach or reload history.
  useEffect(() => {
    if (!session || turnId !== null || holding) return
    const sessionId = session.id
    let inFlight = false
    const tick = async () => {
      if (inFlight || turnIdRef.current || compactingRef.current) return
      const previous = sessionRef.current
      if (!previous || previous.id !== sessionId) return
      inFlight = true
      try {
        const next = await api.getSession(sessionId)
        if (turnIdRef.current || compactingRef.current || sessionRef.current?.id !== sessionId) return
        const action = idleFollow(previous, next)
        if (action.kind === 'none') return
        if (action.kind === 'attach') {
          turnIdRef.current = action.turnId
          setTurnId(action.turnId)
        }
        sessionRef.current = next
        setSession(next)
        setItems(fromHistory(await api.history(sessionId, action.kind === 'attach' ? action.turnId : undefined)))
        if (action.kind === 'attach') {
          void consume(sessionId, await api.attach(sessionId, action.turnId))
        }
      } catch (e) {
        fail(e)
      } finally {
        inFlight = false
      }
    }
    const id = setInterval(() => void tick(), IDLE_POLL_MS)
    return () => clearInterval(id)
  }, [session?.id, turnId, holding, consume, fail])

  // Lines background reflection queued (after a compaction) reach this page by
  // polling. The first poll only records where the queue stands, so notices
  // from before the page opened are not replayed.
  useEffect(() => {
    if (!session) return
    const sessionId = session.id
    let after: number | null = null
    let inFlight = false
    let stopped = false
    const poll = async () => {
      if (inFlight || stopped) return
      inFlight = true
      try {
        const list = await api.notices(sessionId, after ?? 0)
        if (stopped) return
        if (after !== null && list.notices.length > 0) {
          setItems((prev) => [...prev, ...list.notices.map((n) => ({ kind: 'notice' as const, text: n.text }))])
        }
        after = list.last
      } catch {
        // A missed poll is retried; the notices stay queued on the server.
      } finally {
        inFlight = false
      }
    }
    void poll()
    const id = setInterval(() => void poll(), NOTICE_POLL_MS)
    return () => {
      stopped = true
      clearInterval(id)
    }
  }, [session?.id])

  const send = async (text: string, image?: { data: string; mime_type: string }): Promise<void> => {
    if (!session) return
    const command = parseWebCommand(text)
    // An approval is decided inside the running turn, so it is not held back
    // with the next input.
    if (command.kind === 'approve' || command.kind === 'deny') {
      setError('')
      const sessionId = session.id
      const decisionTurnId = turnIdRef.current
      const pendingMessage = approvalMessageRef.current
      if (pendingMessage && pendingMessage.sessionId === sessionId && pendingMessage.turnId === decisionTurnId) {
        approvalMessageRef.current = null
        setQueuedInput(pendingMessage.text)
      }
      try {
        const decision = command.kind === 'approve' ? 'allow' : 'deny'
        await api.decideApproval(sessionId, command.id, decision)
        if (sessionRef.current?.id !== sessionId || turnIdRef.current !== decisionTurnId) return
        const verb = decision === 'allow' ? 'Approved' : 'Denied'
        setItems((prev) => [...prev, { kind: 'notice', text: `${verb} ${command.id}.` }])
      } catch (e) {
        if (sessionRef.current?.id === sessionId && turnIdRef.current === decisionTurnId) fail(e)
      }
      return
    }
    if (turnIdRef.current !== null) {
      if (!image && command.kind === 'prompt') {
        const sessionId = session.id
        const originalTurnId = turnIdRef.current
        const inFlight = approvalMessageRef.current
        if (inFlight && inFlight.sessionId === sessionId && inFlight.turnId === originalTurnId) {
          setQueuedInput(text)
          return
        }
        const request = { sessionId, turnId: originalTurnId, text }
        approvalMessageRef.current = request
        setQueuedInput(text)
        try {
          const reply = await api.approvalMessage(sessionId, text)
          if (approvalMessageRef.current !== request) return
          if (sessionRef.current?.id !== sessionId) return
          if (reply) {
            if (reply.queued) {
              if (turnIdRef.current === null) {
                setQueuedInput((queued) => queued === text ? '' : queued)
                await send(text)
              }
            } else {
              setItems((prev) => [...prev, { kind: 'notice', text: reply.text }])
              setQueuedInput((queued) => queued === text ? '' : queued)
            }
            return
          }
          if (turnIdRef.current !== originalTurnId) {
            if (turnIdRef.current === null) {
              setQueuedInput((queued) => queued === text ? '' : queued)
              await send(text)
            }
            return
          }
        } catch (e) {
          if (approvalMessageRef.current !== request) return
          if (sessionRef.current?.id !== sessionId) return
          fail(e)
          if (turnIdRef.current === originalTurnId) setQueuedInput(text)
          else if (turnIdRef.current === null) {
            setQueuedInput((queued) => queued === text ? '' : queued)
            await send(text)
          }
          return
        } finally {
          if (approvalMessageRef.current === request) approvalMessageRef.current = null
        }
      }
      setQueuedInput(text)
      return
    }
    setError('')
    if (command.kind === 'error') {
      setError(command.message)
      return
    }
    if (command.kind === 'help') {
      setItems((prev) => [...prev, { kind: 'notice', text: helpText() }])
      return
    }
    if (command.kind === 'session') {
      try {
        const fresh = await api.getSession(session.id)
        setSession(fresh)
        setItems((prev) => [...prev, { kind: 'notice', text: sessionText(fresh) }])
      } catch (e) {
        fail(e)
      }
      return
    }
    if (command.kind === 'new') {
      // Keep creating in the currently open session's workspace; the request
      // only needs "workspace" when that differs from the startup one.
      await open(undefined, info && session.workspace !== info.workspace ? session.workspace : undefined)
      return
    }
    if (command.kind === 'resume') {
      setItems((prev) => [...prev, { kind: 'notice', text: 'Choose a session from the sidebar to resume it.' }])
      return
    }
    if (command.kind === 'model') {
      try {
        const fresh = await api.info()
        setInfo(fresh)
        setItems((prev) => [
          ...prev,
          {
            kind: 'notice',
            text: `Model: ${fresh.model}\nProvider: ${fresh.provider}\nProfile: ${fresh.profile}\nProfiles: ${fresh.profiles.join(', ') || '(none)'}`,
          },
        ])
      } catch (e) {
        fail(e)
      }
      return
    }
    if (command.kind === 'rename') {
      try {
        const renamed = await api.renameSession(session.id, command.name)
        setSession(renamed)
        setItems((prev) => [...prev, { kind: 'notice', text: `Renamed session to ${command.name}` }])
        void refreshSessions()
      } catch (e) {
        fail(e)
      }
      return
    }
    if (command.kind === 'compact') {
      await compact(command.focus)
      return
    }
    if (command.kind === 'reflect') {
      await reflect(command.focus)
      return
    }
    if (command.kind === 'generatedSkills') {
      try {
        const list = await api.generatedSkills(session.id)
        setItems((prev) => [...prev, { kind: 'notice', text: generatedSkillsText(list) }])
      } catch (e) {
        fail(e)
      }
      return
    }
    if (command.kind === 'revertSkill') {
      try {
        const r = await api.revertSkill(session.id, command.name)
        const text =
          r.result === 'restored'
            ? `Reverted skill ${r.name} to its previous version; the change applies to new sessions`
            : `Removed skill ${r.name}, which reflection created; the change applies to new sessions`
        setItems((prev) => [...prev, { kind: 'notice', text }])
      } catch (e) {
        fail(e)
      }
      return
    }
    if (command.kind === 'sandbox') {
      setItems((prev) => [...prev, { kind: 'notice', text: `Sandbox: ${session.sandbox.summary}` }])
      return
    }
    if (command.kind === 'sandboxReload') {
      try {
        const sandbox = await api.reloadSandbox()
        const fresh = await api.getSession(session.id)
        setSession(fresh)
        setInfo((prev) => (prev ? { ...prev, sandbox: sandbox.summary } : prev))
        setItems((prev) => [...prev, { kind: 'notice', text: `Sandbox reloaded: ${sandbox.summary}` }])
      } catch (e) {
        fail(e)
      }
      return
    }
    if (command.kind === 'tasks') {
      try {
        const r = await api.listTasks(session.id)
        setTasksKey((k) => k + 1)
        const taskList = r.tasks.length ? r.tasks.map(taskText).join('\n\n') : 'No sub-agent tasks.'
        setItems((prev) => [...prev, { kind: 'notice', text: taskList }])
      } catch (e) {
        fail(e)
      }
      return
    }
    if (command.kind === 'task') {
      try {
        const task = await api.getTask(session.id, command.id)
        setItems((prev) => [...prev, { kind: 'notice', text: taskText(task) }])
      } catch (e) {
        fail(e)
      }
      return
    }
    if (command.kind === 'taskCancel') {
      try {
        await api.cancelTask(session.id, command.id)
        setTasksKey((k) => k + 1)
        setItems((prev) => [...prev, { kind: 'notice', text: `Canceled task ${command.id}` }])
      } catch (e) {
        fail(e)
      }
      return
    }
    if (command.kind === 'mcp') {
      try {
        const r = await api.listMcp(session.id)
        const list = r.servers.length ? r.servers.map(mcpServerLine).join('\n') : 'No MCP servers configured.'
        setItems((prev) => [...prev, { kind: 'notice', text: list }])
      } catch (e) {
        fail(e)
      }
      return
    }
    if (command.kind === 'exit') {
      window.close()
      setItems((prev) => [...prev, { kind: 'notice', text: 'Close this browser tab to exit the Web UI.' }])
      return
    }
    try {
      const res = await api.startTurn(session.id, command.text, image)
      const id = res.headers.get('Otto-Turn-Id')
      if (!id) throw new Error('server response has no Otto-Turn-Id header')
      turnIdRef.current = id
      setTurnId(id)
      // Cleared by consume() at the turn's first frame. The prompt text comes
      // from the turn's user_message frame; only the image is added here.
      setQueuedTurn(true)
      if (image) {
        setItems((prev) => [
          ...prev,
          { kind: 'image', data: image.data, mime_type: image.mime_type, created_at: new Date().toISOString() },
        ])
      }
      await consume(session.id, res)
    } catch (e) {
      // 409 queue_full: the server holds 16 queued turns already.
      fail(e)
    }
  }

  sendRef.current = send

  const cancel = useCallback(() => {
    if (session && turnId) api.cancelTurn(session.id, turnId).catch(fail)
  }, [fail, session, turnId])

  useEffect(() => {
    if (!turnId) return
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== 'Escape' || event.repeat) return
      event.preventDefault()
      cancel()
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [cancel, turnId])

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
      void refreshUsage()
    }
  }

  const reflect = async (focus: string) => {
    if (!session) return
    setError('')
    setReflecting(true)
    const ac = new AbortController()
    reflectAbort.current = ac
    try {
      const r = await api.reflect(session.id, focus, ac.signal)
      setItems((prev) => [...prev, { kind: 'notice', text: r.line }])
    } catch (e) {
      if (!ac.signal.aborted) fail(e)
    } finally {
      if (reflectAbort.current === ac) reflectAbort.current = null
      setReflecting(false)
      void refreshUsage()
    }
  }

  const renameSession = async (name: string) => {
    if (!session) return
    setError('')
    setRenaming(true)
    try {
      const renamed = await api.renameSession(session.id, name)
      setSession(renamed)
      setRenameDraft(null)
      void refreshSessions()
    } catch (e) {
      fail(e)
    } finally {
      setRenaming(false)
    }
  }

  const openRenameDialog = () => {
    if (!session) return
    setRenameDraft(session.name ?? sessionLabel(session.id))
  }

  const renameName = renameDraft?.trim() ?? ''
  const canSaveRename = Boolean(renameName) && !renaming
  const busy = turnId !== null || holding

  return (
    <div className="app">
      <header className="topbar">
        <div className="brand" aria-label="Otto Web UI">
          <img className="brand-mark" src={logo} alt="" />
          <div>
            <div className="brand-name">Otto</div>
            <div className="brand-subtitle">local agent</div>
          </div>
        </div>
        <nav className="view-tabs" aria-label="View">
          <button type="button" aria-pressed={view === 'chat'} onClick={() => setView('chat')}>
            Chat
          </button>
          <button type="button" aria-pressed={view === 'usage'} onClick={() => setView('usage')}>
            Usage
          </button>
          <button type="button" aria-pressed={view === 'workflows'} onClick={() => setView('workflows')}>
            Workflows
          </button>
          <button type="button" aria-pressed={view === 'agents'} onClick={() => setView('agents')}>
            Agents
          </button>
        </nav>
        {(view === 'chat' || view === 'changes') && (
          <button
            type="button"
            className="sidebar-toggle"
            aria-pressed={sidebarOpen}
            onClick={() => setSidebarOpen((v) => !v)}
          >
            Sessions
          </button>
        )}
        <span className="spacer" />
        {view === 'chat' && session && (
          <div className="session-chip" title={session.id}>
            <span>{session.name ?? sessionLabel(session.id)}</span>
            <strong>{workspaceName(session.workspace)}</strong>
            <button type="button" disabled={busy} onClick={openRenameDialog}>
              Rename
            </button>
            <button type="button" aria-pressed={showContext} onClick={() => setShowContext((v) => !v)}>
              Context
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
        {view === 'usage' ? (
          <UsageView onError={fail} />
        ) : view === 'workflows' ? (
          <WorkflowsView onError={fail} />
        ) : view === 'agents' ? (
          <AgentsView
            onError={fail}
            onOpenSession={(id) => {
              setView('chat')
              void open(id)
            }}
          />
        ) : (
          <>
            <div className={`sidebar-wrap${sidebarOpen ? ' open' : ''}`}>
              <Sidebar
                sessions={sessions}
                current={session?.id ?? ''}
                disabled={busy}
                status={status}
                onOpen={(id, workspace) => {
                  setSidebarOpen(false)
                  setView('chat')
                  void open(id, workspace)
                }}
                onOpenChanges={(workspace) => {
                  setSidebarOpen(false)
                  setChangesWorkspace(workspace)
                  setView('changes')
                }}
                onWorkspaceRemoved={() => void refreshSessions()}
              />
            </div>
            {view === 'changes' ? (
              <ChangesView workspace={changesWorkspace} status={status} onError={fail} />
            ) : (
              <TranscriptView items={items} queuedInput={queuedInput} activeSession={session !== null} />
            )}
          </>
        )}
      </main>
      {view === 'chat' && session && showContext && (
        <ContextPanel
          sessionId={session.id}
          refreshKey={tasksKey}
          onClose={() => setShowContext(false)}
          onError={fail}
        />
      )}
      {view === 'chat' && session && <Tasks sessionId={session.id} refreshKey={tasksKey} onError={fail} />}
      {view === 'chat' && (
        <Composer
          key={session?.id ?? 'closed'}
          disabled={!session}
          running={turnId !== null}
          compacting={compacting}
          reflecting={reflecting}
          queuedText={queuedInput}
          onSend={send}
          onQueue={(t) => void send(t)}
          onWithdrawQueue={() => setQueuedInput('')}
          onCancel={cancel}
          onCompact={compact}
        />
      )}
      {renameDraft !== null && (
        <div className="modal-backdrop" onMouseDown={() => !renaming && setRenameDraft(null)}>
          <form
            className="rename-dialog"
            role="dialog"
            aria-modal="true"
            aria-labelledby="rename-title"
            onMouseDown={(e) => e.stopPropagation()}
            onSubmit={(e) => {
              e.preventDefault()
              if (canSaveRename) void renameSession(renameName)
            }}
            onKeyDown={(e) => {
              if (e.key === 'Escape' && !renaming) setRenameDraft(null)
            }}
          >
            <div className="rename-dialog-icon" aria-hidden="true">
              ✦
            </div>
            <div className="rename-dialog-copy">
              <h2 id="rename-title">Rename session</h2>
              <p>Give this workspace thread a short, memorable name.</p>
            </div>
            <label className="rename-field">
              <span>Session name</span>
              <input
                autoFocus
                value={renameDraft}
                maxLength={80}
                placeholder="e.g. Release notes polish"
                onChange={(e) => setRenameDraft(e.target.value)}
              />
            </label>
            <div className="rename-actions">
              <button type="button" className="secondary" disabled={renaming} onClick={() => setRenameDraft(null)}>
                Cancel
              </button>
              <button type="submit" className="primary" disabled={!canSaveRename}>
                {renaming ? 'Saving…' : 'Save name'}
              </button>
            </div>
          </form>
        </div>
      )}
      <Footer
        info={info}
        session={session}
        turnUsage={turnUsage}
        recordedUsage={recordedUsage}
        queued={queuedTurn}
        status={
          phaseState &&
          statusLine(
            (phaseState.retryEvent ? phase(phaseState.retryEvent, Math.max(0, now - phaseState.since)) : null) ?? phaseState.name,
            Math.max(0, Math.floor((now - phaseState.since) / 1000)),
            Math.max(0, Math.floor((now - phaseState.turnStart) / 1000)),
          )
        }
      />
    </div>
  )
}
