import { useCallback, useEffect, useState } from 'react'
import { api, type WorkflowRun, type WorkflowView } from './api'

const active = (status: WorkflowRun['status']) => status === 'running' || status === 'waiting' || status === 'paused'
const stepLabel = (step: WorkflowRun['steps'][number]) => {
  if (step.kind === 'approval') return 'human approval'
  if (step.kind === 'handoff') return `handoff to ${step.agent}`
  return step.agent
}

export function WorkflowsView({ onError }: { onError: (error: unknown) => void }) {
  const [runs, setRuns] = useState<WorkflowRun[]>([])
  const [selected, setSelected] = useState<WorkflowView | null>(null)
  const [name, setName] = useState('')
  const [input, setInput] = useState('')

  const load = useCallback(async () => {
    try {
      const next = await api.listWorkflows()
      setRuns(next.runs)
      if (selected) setSelected(await api.getWorkflow(selected.run.id))
    } catch (error) {
      onError(error)
    }
  }, [onError, selected?.run.id])

  useEffect(() => {
    void load()
    const id = setInterval(() => void load(), 1500)
    return () => clearInterval(id)
  }, [load])

  const start = async () => {
    if (!name.trim()) return
    try {
      const view = await api.startWorkflow(name.trim(), input)
      setSelected(view)
      setName('')
      setInput('')
      void load()
    } catch (error) {
      onError(error)
    }
  }

  const update = async (operation: Promise<WorkflowView>) => {
    try {
      setSelected(await operation)
      void load()
    } catch (error) {
      onError(error)
    }
  }

  return (
    <section className="workflows-view" aria-labelledby="workflows-title">
      <header className="workflows-heading">
        <div>
          <h1 id="workflows-title">Workflows</h1>
          <p>Durable DAG runs resume only from committed step boundaries.</p>
        </div>
        <div className="workflow-start">
          <input aria-label="Workflow name" placeholder="workflow name" value={name} onChange={(event) => setName(event.target.value)} />
          <input aria-label="Workflow input" placeholder="input (optional)" value={input} onChange={(event) => setInput(event.target.value)} />
          <button className="primary" type="button" disabled={!name.trim()} onClick={() => void start()}>Run</button>
        </div>
      </header>

      <div className="workflow-layout">
        <nav className="workflow-runs" aria-label="Workflow runs">
          {runs.length === 0 ? <p>No workflow runs.</p> : runs.map((run) => (
            <button key={run.id} type="button" aria-pressed={selected?.run.id === run.id} onClick={() => api.getWorkflow(run.id).then(setSelected).catch(onError)}>
              <strong>{run.workflow}</strong>
              <span>{run.status} · {run.id.slice(0, 8)}</span>
            </button>
          ))}
        </nav>

        <div className="workflow-detail">
          {!selected ? <p>Select a run.</p> : (
            <>
              <header>
                <div>
                  <h2>{selected.run.workflow}</h2>
                  <code>{selected.run.id}</code>
                </div>
                <span className={`workflow-status ${selected.run.status}`}>{selected.run.status}</span>
                {selected.run.status === 'running' && (
                  <button type="button" onClick={() => void update(api.resumeWorkflow(selected.run.id))}>Resume</button>
                )}
                {active(selected.run.status) && (
                  <button className="danger" type="button" onClick={() => void update(api.cancelWorkflow(selected.run.id))}>Cancel</button>
                )}
              </header>

              {selected.requests.filter((request) => request.status === 'pending').map((request) => (
                <section className="workflow-approval" key={request.id}>
                  <strong>Approval required · {request.step_id}</strong>
                  <p>{request.prompt}</p>
                  <button className="primary" type="button" onClick={() => void update(api.approveWorkflow(request.id))}>Approve</button>
                  <button className="danger" type="button" onClick={() => void update(api.rejectWorkflow(request.id))}>Reject</button>
                </section>
              ))}

              <ol className="workflow-steps">
                {selected.run.steps.map((step) => (
                  <li key={step.id} className={step.status}>
                    <div>
                      <strong>{step.id}</strong>
                      <span>{stepLabel(step)} · {step.status} · attempt {step.attempt}</span>
                    </div>
                    {step.status === 'interrupted' && (
                      <button type="button" onClick={() => void update(api.resumeWorkflow(selected.run.id, step.id))}>Retry</button>
                    )}
                    {(step.error || step.result) && <pre>{step.error || step.result}</pre>}
                  </li>
                ))}
              </ol>
            </>
          )}
        </div>
      </div>
    </section>
  )
}
