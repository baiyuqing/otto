import type { SessionListRow } from './types'
import { sessionLabel } from './uiText'

export function SessionPicker(props: {
  sessions: SessionListRow[]
  current: string
  disabled: boolean
  onOpen: (id?: string) => void
}) {
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
          <option key={s.id} value={s.id}>
            {s.name ?? sessionLabel(s.id)} {s.open ? '●' : ''} {s.model ?? ''}
          </option>
        ))}
      </select>
      <button className="primary" disabled={props.disabled} onClick={() => props.onOpen()}>
        New session
      </button>
    </div>
  )
}
