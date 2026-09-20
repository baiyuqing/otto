import type { Info, Session, Usage } from './wire'
import type { UsageSummary } from './api'

const n = (v: number | undefined) => (v ?? 0).toLocaleString()

// Footer shows the process (info) and the open session's cumulative usage
// and context size. turnUsage is the running total of provider_usage events
// for the turn in flight, cleared when the session is re-read at turn end.
export function Footer(props: {
  info: Info | null
  session: Session | null
  turnUsage: Usage | null
  recordedUsage: UsageSummary | null
}) {
  const { info, session, turnUsage, recordedUsage } = props
  return (
    <div className="footer">
      {info && (
        <span>
          {info.provider} · {info.model} · thinking {info.thinking || 'default'} · {info.sandbox}
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
      {recordedUsage && (
        <span>
          recorded in {n(recordedUsage.input_tokens)} out {n(recordedUsage.output_tokens)} cached{' '}
          {n(recordedUsage.cached_input_tokens)} ({n(recordedUsage.cache_hit_rate * 100)}% hit)
        </span>
      )}
    </div>
  )
}
