import { useEffect, useState } from 'react'
import type { SessionListRow } from './wire'
import { api, type WorkspaceEntry } from './api'
import { sessionLabel, workspaceName } from './uiText'

export function SessionPicker(props: {
  sessions: SessionListRow[]
  current: string
  disabled: boolean
  onOpen: (id?: string, workspace?: string) => void
}) {
  const [startup, setStartup] = useState('')
  const [workspaces, setWorkspaces] = useState<WorkspaceEntry[]>([])
  const [selected, setSelected] = useState('')
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
        setSelected(list.startup)
      })
      .catch(() => {
        // The picker still works with just the current session's workspace.
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
      setSelected(entry.path)
      setNewPath('')
    } catch (e) {
      setAddError(e instanceof Error ? e.message : String(e))
    }
  }

  return (
    <div className="session-picker">
      <select
        aria-label="Open session"
        value={props.current}
        disabled={props.disabled}
        onChange={(e) => e.target.value && props.onOpen(e.target.value)}
      >
        <option value="">Select session…</option>
        {props.sessions.map((s) => (
          <option key={s.id} value={s.id} title={s.workspace || undefined}>
            {s.name ?? sessionLabel(s.id)} {s.open ? '●' : ''} {s.model ?? ''}
            {s.workspace ? ` · ${workspaceName(s.workspace)}` : ''}
          </option>
        ))}
      </select>
      <select
        aria-label="Workspace"
        value={selected}
        disabled={props.disabled}
        onChange={(e) => setSelected(e.target.value)}
      >
        {workspaces.map((w) => (
          <option key={w.path} value={w.path}>
            {workspaceName(w.path)}
          </option>
        ))}
      </select>
      <input
        aria-label="Add workspace path"
        placeholder="Add workspace path…"
        value={newPath}
        disabled={props.disabled}
        onChange={(e) => setNewPath(e.target.value)}
      />
      <button disabled={props.disabled} onClick={() => void addWorkspace()}>
        Add workspace
      </button>
      {addError && <span role="alert">{addError}</span>}
      <button
        className="primary"
        disabled={props.disabled}
        onClick={() => props.onOpen(undefined, selected && selected !== startup ? selected : undefined)}
      >
        New session
      </button>
    </div>
  )
}
