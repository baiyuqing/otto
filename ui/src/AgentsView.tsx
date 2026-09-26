import { useCallback, useEffect, useState } from 'react'
import { api, type AgentTask, type AgentTaskDetail } from './api'
import { TranscriptView } from './TranscriptView'
import { fromHistory } from './wire'
import { sessionLabel, workspaceName } from './uiText'

const active = (t: AgentTask) => t.status === 'queued' || t.status === 'running'
const n = (v: number) => v.toLocaleString()

const statuses = ['', 'queued', 'running', 'succeeded', 'failed', 'canceled', 'interrupted']

function duration(t: AgentTask): string {
  if (!t.started_at) return '-'
  const start = Date.parse(t.started_at)
  const end = t.finished_at ? Date.parse(t.finished_at) : Date.now()
  const seconds = Math.max(0, Math.round((end - start) / 1000))
  return seconds < 60 ? `${seconds}s` : `${Math.floor(seconds / 60)}m${seconds % 60}s`
}

const key = (t: AgentTask) => `${t.parent_session}:${t.task_id}`

// AgentsView lists sub-agent tasks from every session of every otto process
// on the machine (GET /v1/tasks, backed by ~/.otto/tasks.db), independent of
// the currently open chat session.
export function AgentsView(props: { onError: (e: unknown) => void; onOpenSession: (id: string) => void }) {
  const { onError, onOpenSession } = props
  const [tasks, setTasks] = useState<AgentTask[]>([])
  const [nextBefore, setNextBefore] = useState('')
  const [status, setStatus] = useState('')
  const [workspace, setWorkspace] = useState('')
  const [workspaceDraft, setWorkspaceDraft] = useState('')
  const [sessionIds, setSessionIds] = useState<Set<string>>(new Set())
  const [selected, setSelected] = useState<AgentTaskDetail | null>(null)

  useEffect(() => {
    api
      .listSessions()
      .then((r) => setSessionIds(new Set(r.sessions.map((s) => s.id))))
      .catch(onError)
  }, [onError])

  const load = useCallback(
    (opts: { before?: string; limit?: number } = {}) =>
      api
        .listAgentTasks({ status: status || undefined, workspace: workspace || undefined, limit: opts.limit, before: opts.before })
        .then((r) => {
          setTasks((prev) => (opts.before ? [...prev, ...r.tasks] : r.tasks))
          setNextBefore(r.next_before)
        })
        .catch(onError),
    [status, workspace, onError],
  )

  useEffect(() => {
    void load()
  }, [load])

  const polling = tasks.some(active)
  useEffect(() => {
    if (!polling) return
    const id = setInterval(() => void load({ limit: Math.max(tasks.length, 100) }), 3000)
    return () => clearInterval(id)
  }, [polling, load, tasks.length])

  const selectTask = (t: AgentTask) => api.getAgentTask(t.parent_session, t.task_id).then(setSelected).catch(onError)
  const closeTask = useCallback(() => setSelected(null), [])

  useEffect(() => {
    if (!selected) return
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') closeTask()
    }
    document.addEventListener('keydown', onKeyDown)
    return () => document.removeEventListener('keydown', onKeyDown)
  }, [closeTask, selected])

  const cancel = (t: AgentTask) =>
    api
      .cancelTask(t.parent_session, t.task_id)
      .then(() => selectTask(t))
      .then(() => load())
      .catch(onError)

  return (
    <section className="agents-view" aria-labelledby="agents-title">
      <header className="agents-heading">
        <div>
          <h1 id="agents-title">Agents</h1>
          <p>Sub-agent tasks from every session on this machine.</p>
        </div>
        <div className="agents-filters">
          <label>
            Status
            <select value={status} onChange={(e) => setStatus(e.target.value)}>
              {statuses.map((s) => (
                <option key={s || 'all'} value={s}>
                  {s || 'all'}
                </option>
              ))}
            </select>
          </label>
          <form
            onSubmit={(e) => {
              e.preventDefault()
              setWorkspace(workspaceDraft.trim())
            }}
          >
            <input
              aria-label="Workspace filter"
              placeholder="workspace path"
              value={workspaceDraft}
              onChange={(e) => setWorkspaceDraft(e.target.value)}
            />
            <button type="submit">Filter</button>
          </form>
        </div>
      </header>

      <div className="agents-layout">
        <div className="agents-table-wrap">
          {tasks.length === 0 ? (
            <p>No sub-agent tasks.</p>
          ) : (
            <table className="agents-table">
              <thead>
                <tr>
                  <th>Status</th>
                  <th>Agent</th>
                  <th>Description</th>
                  <th>Workspace</th>
                  <th>Parent session</th>
                  <th>Created</th>
                  <th>Duration</th>
                  <th>Steps</th>
                  <th>Tool calls</th>
                  <th>Tokens</th>
                </tr>
              </thead>
              <tbody>
                {tasks.map((t) => (
                  <tr
                    key={key(t)}
                    className={t.status}
                    tabIndex={0}
                    role="button"
                    aria-pressed={selected?.task.parent_session === t.parent_session && selected?.task.task_id === t.task_id}
                    onClick={() => void selectTask(t)}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter' || e.key === ' ') {
                        e.preventDefault()
                        void selectTask(t)
                      }
                    }}
                  >
                    <td>{t.status}</td>
                    <td>{t.agent || 'default'}</td>
                    <td>{t.description}</td>
                    <td title={t.workspace}>{workspaceName(t.workspace)}</td>
                    <td>{sessionLabel(t.parent_session)}</td>
                    <td>{t.created_at}</td>
                    <td>{duration(t)}</td>
                    <td>{t.steps}</td>
                    <td>{t.tool_calls}</td>
                    <td>{n(t.input_tokens + t.output_tokens)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
          {nextBefore && (
            <button type="button" onClick={() => void load({ before: nextBefore })}>
              Load more
            </button>
          )}
        </div>

        <div className="agents-detail">
          {!selected ? (
            <p>Select a task.</p>
          ) : (
            <>
              <header>
                <div>
                  <h2>{selected.task.name ?? (selected.task.agent || 'default')}</h2>
                  <code>{selected.task.task_id}</code>
                </div>
                <span className={`agents-status ${selected.task.status}`}>{selected.task.status}</span>
                {sessionIds.has(selected.task.parent_session) ? (
                  <button type="button" onClick={() => onOpenSession(selected.task.parent_session)}>
                    Open session
                  </button>
                ) : (
                  <span title={selected.task.parent_session_path}>
                    {selected.task.parent_session_path || sessionLabel(selected.task.parent_session)}
                  </span>
                )}
                {selected.task.cancelable && (
                  <button type="button" className="danger" onClick={() => void cancel(selected.task)}>
                    Cancel
                  </button>
                )}
                <button type="button" className="agents-detail-close" onClick={closeTask} aria-label="Close task detail">
                  ×
                </button>
              </header>
              <p>{selected.task.prompt}</p>
              {(selected.task.error || selected.task.result) && <pre>{selected.task.error || selected.task.result}</pre>}
              {selected.transcript_missing ? (
                <p>Transcript not available.</p>
              ) : (
                <TranscriptView items={fromHistory(JSON.stringify(selected.history))} activeSession={false} />
              )}
            </>
          )}
        </div>
      </div>
    </section>
  )
}
