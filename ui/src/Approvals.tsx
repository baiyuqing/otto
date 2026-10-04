import { useEffect, useState } from 'react'
import { api, type ApprovalRequest } from './api'

export function Approvals({ sessionId }: { sessionId: string }) {
  const [requests, setRequests] = useState<ApprovalRequest[]>([])
  const [feedback, setFeedback] = useState<Record<string, string>>({})
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState('')
  const [outcome, setOutcome] = useState<ApprovalRequest | null>(null)
  useEffect(() => {
    if (!outcome || !['not_started', 'applying'].includes(outcome.application)) return
    let disposed = false
    let timer: ReturnType<typeof setTimeout>
    const refresh = async () => {
      try {
        const current = await api.sharedApproval(sessionId, outcome.id)
        if (disposed) return
        setOutcome(current)
        if (['not_started', 'applying'].includes(current.application)) timer = setTimeout(() => void refresh(), 500)
      } catch (e) {
        if (!disposed) {
          setError(e instanceof Error ? e.message : String(e))
          timer = setTimeout(() => void refresh(), 500)
        }
      }
    }
    timer = setTimeout(() => void refresh(), 500)
    return () => { disposed = true; clearTimeout(timer) }
  }, [sessionId, outcome?.id])
  useEffect(() => {
    let disposed = false
    let timer: ReturnType<typeof setTimeout>
    const refresh = async () => {
      try {
        const snapshot = await api.approvals(sessionId)
        if (!disposed) { setRequests(snapshot.requests); setError('') }
      } catch (e) {
        if (!disposed) setError(e instanceof Error ? e.message : String(e))
      } finally {
        if (!disposed) timer = setTimeout(() => void refresh(), 500)
      }
    }
    void refresh()
    return () => { disposed = true; clearTimeout(timer) }
  }, [sessionId])
  const decide = async (request: ApprovalRequest, option: 'approve' | 'reject' | 'withdraw') => {
    setBusy(request.id)
    try {
      const decided = await api.decideSharedApproval(sessionId, request.id, request.revision, option, feedback[request.id] ?? '')
      setRequests((rows) => rows.filter((r) => r.id !== request.id))
      setOutcome(decided)
    } catch (e) { setError(e instanceof Error ? e.message : String(e)) }
    finally { setBusy(null) }
  }
  if (!requests.length && !error && !outcome) return null
  return <section className="approvals" aria-label="Approvals">
    {error && <p role="alert">{error}</p>}
    {outcome && <p role="status">{outcome.title}: {outcome.state}{outcome.application !== 'not_required' ? ` · application: ${outcome.application}` : ''}{outcome.application_error ? ` · ${outcome.application_error}` : ''}</p>}
    {requests.map((request) => <article key={request.id}>
      <strong>{request.title}</strong>
      <p>{request.kind === 'confirmation' ? 'Proposal confirmation' : request.persistent ? 'Persistent authorization' : 'Operation authorization'}</p>
      <pre>{request.subject.details}</pre>
      <p>{request.reason}</p>
      <label>Feedback <textarea value={feedback[request.id] ?? ''} onChange={(e) => setFeedback((prev) => ({ ...prev, [request.id]: e.target.value }))} /></label>
      <div>
        <button disabled={busy !== null} onClick={() => void decide(request, 'reject')}>Reject</button>
        <button disabled={busy !== null} onClick={() => void decide(request, 'approve')}>Approve</button>
        <button disabled={busy !== null} onClick={() => void decide(request, 'withdraw')}>Withdraw</button>
      </div>
    </article>)}
  </section>
}
