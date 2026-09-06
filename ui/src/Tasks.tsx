import { useCallback, useEffect, useState } from 'react'
import { api } from './api'
import type { Task } from './types'

const active = (t: Task) => t.status === 'queued' || t.status === 'running'

// Tasks lists the session's sub-agent tasks. It re-reads on mount, whenever
// refreshKey changes (the parent bumps it on notification events and at turn
// end), and every 3 s while any task is queued or running.
export function Tasks(props: { sessionId: string; refreshKey: number; onError: (e: unknown) => void }) {
  const { sessionId, refreshKey, onError } = props
  const [tasks, setTasks] = useState<Task[]>([])

  const load = useCallback(
    () =>
      api
        .listTasks(sessionId)
        .then((r) => setTasks(r.tasks))
        .catch(onError),
    [sessionId, onError],
  )

  useEffect(() => {
    void load()
  }, [load, refreshKey])

  const polling = tasks.some(active)
  useEffect(() => {
    if (!polling) return
    const id = setInterval(() => void load(), 3000)
    return () => clearInterval(id)
  }, [polling, load])

  if (tasks.length === 0) return null
  return (
    <details className="tasks" open={polling}>
      <summary>
        Tasks: {tasks.filter(active).length} active / {tasks.length}
      </summary>
      <ul>
        {tasks.map((t) => (
          <li key={t.id} className={t.status}>
            <code>{t.id.slice(0, 8)}</code> {t.name ?? t.agent} · {t.status} · {t.steps} steps
            {t.last_tool && ` · ${t.last_tool}`}
            {active(t) && (
              <button onClick={() => api.cancelTask(sessionId, t.id).then(load).catch(onError)}>Cancel</button>
            )}
            {(t.error || t.result) && <pre>{t.error || t.result}</pre>}
          </li>
        ))}
      </ul>
    </details>
  )
}
