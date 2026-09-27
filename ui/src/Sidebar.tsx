import { useEffect, useState } from 'react'
import type { SessionListRow } from './wire'
import { api, ApiError, type SessionStatus, type WorkspaceEntry } from './api'
import { desktopOpenFolder } from './desktop'
import { FolderPicker } from './FolderPicker'
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
  onOpenChanges?: (workspace: string) => void
  onWorkspaceRemoved?: () => void
  status?: Map<string, SessionStatus>
}) {
  const [startup, setStartup] = useState('')
  const [workspaces, setWorkspaces] = useState<WorkspaceEntry[]>([])
  const [newPath, setNewPath] = useState('')
  const [addError, setAddError] = useState('')
  const [browsing, setBrowsing] = useState(false)
  // A path the server refused as not admitted, waiting on the user's
  // confirmation to trust it.
  const [confirmTrust, setConfirmTrust] = useState<string | null>(null)

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

  // addWorkspace registers path. A folder outside the startup workspace,
  // the configured roots, and every trusted directory is refused with 403
  // WORKSPACE_NOT_ADMITTED; the user is then asked to trust it, and the
  // retry sends trust so the server records it before loading.
  const addWorkspace = async (path: string, trust: boolean) => {
    setAddError('')
    try {
      const entry = await api.addWorkspace(path, trust)
      setWorkspaces((prev) => (prev.some((w) => w.path === entry.path) ? prev : [...prev, entry]))
      setNewPath('')
    } catch (e) {
      if (!trust && e instanceof ApiError && e.code === 'WORKSPACE_NOT_ADMITTED') {
        setConfirmTrust(path)
        return
      }
      setAddError(e instanceof Error ? e.message : String(e))
    }
  }

  const chooseFolder = () => {
    setAddError('')
    const openFolder = desktopOpenFolder()
    if (openFolder) openFolder()
    else setBrowsing(true)
  }

  const removeWorkspace = async (path: string) => {
    setAddError('')
    try {
      await api.removeWorkspace(path)
      setWorkspaces((prev) => prev.filter((w) => w.path !== path))
      props.onWorkspaceRemoved?.()
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
                  onClick={() => props.onOpenChanges?.(group.path)}
                >
                  Changes
                </button>
                <button
                  type="button"
                  disabled={props.disabled}
                  onClick={() => props.onOpen(undefined, group.path === startup ? undefined : group.path)}
                >
                  New session
                </button>
                {group.path !== startup && (
                  <button type="button" disabled={props.disabled} onClick={() => void removeWorkspace(group.path)}>
                    Remove
                  </button>
                )}
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
        <button type="button" className="primary" disabled={props.disabled} onClick={chooseFolder}>
          Add workspace…
        </button>
        <details>
          <summary>Enter a path</summary>
          <form
            onSubmit={(e) => {
              e.preventDefault()
              const path = newPath.trim()
              if (path) void addWorkspace(path, false)
            }}
          >
            <input
              aria-label="Add workspace path"
              placeholder="/absolute/path"
              value={newPath}
              disabled={props.disabled}
              onChange={(e) => setNewPath(e.target.value)}
            />
            <button type="submit" disabled={props.disabled}>
              Add path
            </button>
          </form>
        </details>
        {addError && <span role="alert">{addError}</span>}
      </div>
      {browsing && (
        <FolderPicker
          onCancel={() => setBrowsing(false)}
          onChoose={(path) => {
            setBrowsing(false)
            void addWorkspace(path, false)
          }}
        />
      )}
      {confirmTrust !== null && (
        <div className="modal-backdrop" onMouseDown={() => setConfirmTrust(null)}>
          <div
            className="rename-dialog trust-dialog"
            role="dialog"
            aria-modal="true"
            aria-labelledby="trust-title"
            onMouseDown={(e) => e.stopPropagation()}
            onKeyDown={(e) => {
              if (e.key === 'Escape') setConfirmTrust(null)
            }}
          >
            <div className="rename-dialog-copy">
              <h2 id="trust-title">Trust this folder?</h2>
              <p>
                Otto will read and edit files and run commands in <code>{confirmTrust}</code>. It is recorded as
                trusted in your config, the same as <code>otto trust</code>.
              </p>
            </div>
            <div className="rename-actions">
              <button type="button" className="secondary" onClick={() => setConfirmTrust(null)}>
                Cancel
              </button>
              <button
                type="button"
                className="primary"
                autoFocus
                onClick={() => {
                  const path = confirmTrust
                  setConfirmTrust(null)
                  void addWorkspace(path, true)
                }}
              >
                Trust and add
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  )
}
