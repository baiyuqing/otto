import { useEffect, useState } from 'react'
import type { SessionListRow } from './wire'
import { api, type SessionStatus, type WorkspaceEntry } from './api'
import { sessionLabel, workspaceName } from './uiText'

export interface SidebarGroup {
  path: string
  sessions: SessionListRow[]
}

// groupSessions builds one group per working directory: the union of the
// loaded workspaces and the distinct workspace values sessions report, so a
// session row is never dropped when the two disagree. The startup workspace
// sorts first, the rest by path; each group's sessions keep server order.
export function groupSessions(startup: string, workspaces: WorkspaceEntry[], sessions: SessionListRow[]): SidebarGroup[] {
  const paths = new Set<string>()
  if (startup) paths.add(startup)
  for (const w of workspaces) paths.add(w.path)
  for (const s of sessions) if (s.workspace) paths.add(s.workspace)

  const rest = [...paths].filter((p) => p !== startup).sort()
  const ordered = startup ? [startup, ...rest] : rest

  return ordered.map((path) => ({
    path,
    sessions: sessions.filter((s) => (s.workspace || startup) === path),
  }))
}

export function Sidebar(props: {
  sessions: SessionListRow[]
  current: string
  disabled: boolean
  onOpen: (id?: string, workspace?: string) => void
  status?: Map<string, SessionStatus>
}) {
  const [startup, setStartup] = useState('')
  const [workspaces, setWorkspaces] = useState<WorkspaceEntry[]>([])
  const [newPath, setNewPath] = useState('')
  const [addError, setAddError] = useState('')

  useEffect(() => {
    let canceled = false
    api
      .listWorkspaces()
      .then((list) => {
        if (canceled) return
        setStartup(list.startup)
        setWorkspaces(list.workspaces)
      })
      .catch(() => {
        // The sidebar still works with just the current session's workspace.
      })
    return () => {
      canceled = true
    }
  }, [])

  const addWorkspace = async () => {
    const path = newPath.trim()
    if (!path) return
    setAddError('')
    try {
      const entry = await api.addWorkspace(path)
      setWorkspaces((prev) => (prev.some((w) => w.path === entry.path) ? prev : [...prev, entry]))
      setNewPath('')
    } catch (e) {
      setAddError(e instanceof Error ? e.message : String(e))
    }
  }

  const groups = groupSessions(startup, workspaces, props.sessions)

  return (
    <div className="sidebar">
      <div className="sidebar-groups">
        {groups.map((group) => {
          const runningCount = group.sessions.filter((s) => props.status?.get(s.id)?.turn === 'running').length
          return (
            <div className="sidebar-group" key={group.path}>
              <div className="sidebar-group-header" title={group.path}>
                <span>{workspaceName(group.path)}</span>
                {runningCount > 0 && <span className="status-running-count">{runningCount} running</span>}
                <button
                  type="button"
                  disabled={props.disabled}
                  onClick={() => props.onOpen(undefined, group.path === startup ? undefined : group.path)}
                >
                  New session
                </button>
              </div>
              {group.sessions.map((s) => {
                const st = props.status?.get(s.id)
                return (
                  <button
                    type="button"
                    key={s.id}
                    className="sidebar-session"
                    disabled={props.disabled}
                    aria-current={s.id === props.current ? 'true' : undefined}
                    onClick={() => props.onOpen(s.id)}
                  >
                    <span>{s.name ?? sessionLabel(s.id)}</span>
                    {s.open ? <span>●</span> : null}
                    {st?.turn === 'running' && (
                      <span className="status-badge status-running" aria-label="Turn running">
                        running
                      </span>
                    )}
                    {st && st.approvals > 0 && (
                      <span className="status-badge status-approval" aria-label="Approval pending">
                        approval
                      </span>
                    )}
                    {st?.turn === 'error' && (
                      <span className="status-badge status-error" aria-label="Turn failed">
                        error
                      </span>
                    )}
                    {st && st.tasks > 0 && (
                      <span className="status-badge status-tasks" aria-label={`${st.tasks} sub-agent tasks running`}>
                        {st.tasks} tasks
                      </span>
                    )}
                    {s.model && <span>{s.model}</span>}
                  </button>
                )
              })}
            </div>
          )
        })}
      </div>
      <div className="sidebar-add">
        <input
          aria-label="Add workspace path"
          placeholder="Add workspace path…"
          value={newPath}
          disabled={props.disabled}
          onChange={(e) => setNewPath(e.target.value)}
        />
        <button type="button" disabled={props.disabled} onClick={() => void addWorkspace()}>
          Add workspace
        </button>
        {addError && <span role="alert">{addError}</span>}
      </div>
    </div>
  )
}
