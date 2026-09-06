import type { SessionListRow } from './types'

export function SessionPicker(props: {
  sessions: SessionListRow[]
  current: string
  disabled: boolean
  onOpen: (id?: string) => void
}) {
  return (
    <>
      <select
        value={props.current}
        disabled={props.disabled}
        onChange={(e) => e.target.value && props.onOpen(e.target.value)}
      >
        <option value="">Select a session…</option>
        {props.sessions.map((s) => (
          <option key={s.id} value={s.id}>
            {s.id.slice(0, 8)} {s.open ? '●' : ''} {s.model ?? ''}
          </option>
        ))}
      </select>
      <button disabled={props.disabled} onClick={() => props.onOpen()}>
        New session
      </button>
    </>
  )
}
