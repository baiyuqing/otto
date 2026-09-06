import type { Info, Session, Usage } from './types'

const n = (v: number | undefined) => (v ?? 0).toLocaleString()

// Footer shows the process (info) and the open session's cumulative usage
// and context size. turnUsage is the running total of provider_usage events
// for the turn in flight, cleared when the session is re-read at turn end.
export function Footer(props: { info: Info | null; session: Session | null; turnUsage: Usage | null }) {
  const { info, session, turnUsage } = props
  return (
    <div className="footer">
      {info && (
        <span>
          {info.provider} · {info.model} · {info.sandbox}
        </span>
      )}
      {session && (
        <span>
          context {n(session.context_input_tokens)}
          {session.context_window > 0 && ` / ${n(session.context_window)}`} · session in {n(session.usage.input_tokens)} out{' '}
          {n(session.usage.output_tokens)} cached {n(session.usage.cached_input_tokens)}
        </span>
      )}
      {turnUsage && (
        <span>
          turn in {n(turnUsage.input_tokens)} out {n(turnUsage.output_tokens)}
        </span>
      )}
    </div>
  )
}
