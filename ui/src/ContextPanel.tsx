import { useCallback, useEffect, useState } from 'react'
import { api } from './api'
import type { ContextReport, ContextSection } from './wire'

const n = (v: number) => v.toLocaleString()

const sectionName = (s: ContextSection) =>
  ({
    system_prompt: 'System prompt',
    tools: `Tools (built-in, ${s.items.length})`,
    mcp_tools: `Tools (MCP, ${s.items.length})`,
    compaction_summary: 'Compaction summary',
    memory: 'Memory (last turn)',
    messages: `Messages (${s.items.length})`,
  })[s.kind]

// ContextPanel shows what the session's next provider request contains, part
// by part, with estimated tokens and the text of each part. It re-reads the
// report on mount and whenever refreshKey changes (the parent bumps it at
// turn end).
export function ContextPanel(props: {
  sessionId: string
  refreshKey: number
  onClose: () => void
  onError: (e: unknown) => void
}) {
  const { sessionId, refreshKey, onClose, onError } = props
  const [report, setReport] = useState<ContextReport | null>(null)

  const load = useCallback(() => api.context(sessionId).then(setReport).catch(onError), [sessionId, onError])
  useEffect(() => {
    void load()
  }, [load, refreshKey])

  const total = report?.sections.reduce((sum, s) => sum + s.tokens, 0) ?? 0
  return (
    <aside className="context-panel" aria-label="Context">
      <div className="context-head">
        <strong>Context</strong>
        <button type="button" aria-label="Close context" onClick={onClose}>
          ×
        </button>
      </div>
      {report && (
        <>
          <p className="context-summary">
            {report.model} · ~{n(report.estimated_total)}
            {report.context_window > 0 && ` / ${n(report.context_window)}`} tokens (estimate)
            {report.reported_input_tokens !== null && ` · last reported ${n(report.reported_input_tokens)}`}
            {report.compaction_threshold > 0 && ` · compacts at ${n(report.compaction_threshold)}`}
          </p>
          {report.sections.map((s) => (
            <details key={s.kind} className="context-section">
              <summary>
                <span>{sectionName(s)}</span>
                <span className="context-tokens">{n(s.tokens)}</span>
                <meter min={0} max={total} value={s.tokens} />
              </summary>
              {s.items.map((item, i) => (
                <details key={i} className="context-item">
                  <summary>
                    <span>{item.label}</span>
                    <span className="context-tokens">{n(item.tokens)}</span>
                  </summary>
                  <pre>{item.text}</pre>
                </details>
              ))}
            </details>
          ))}
        </>
      )}
    </aside>
  )
}
