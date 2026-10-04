import { useEffect, useRef, useState } from 'react'
import { api } from './api'
import { startSchedulePolling } from './schedule'
import { MarkdownView } from './TranscriptView'

// Scheduled deliveries addressed to the Web UI. They are shown above the
// workspace, so they are visible whichever session or view is open.
export function ScheduledNotices() {
  const [items, setItems] = useState<{ key: number; text: string }[]>([])
  const seq = useRef(0)
  useEffect(
    () =>
      startSchedulePolling({
        claim: api.scheduleClaim,
        delivered: api.scheduleDelivered,
        show: (item) => setItems((prev) => [...prev, { key: seq.current++, text: item.text }]),
        onError: (err) => console.warn('scheduled deliveries:', err),
      }),
    [],
  )
  if (items.length === 0) return null
  return (
    <div className="scheduled-notices" role="status" aria-live="polite">
      {items.map((item) => (
        <div className="scheduled-notice" key={item.key}>
          <MarkdownView text={item.text} />
          <button aria-label="Dismiss notice" onClick={() => setItems((prev) => prev.filter((n) => n.key !== item.key))}>
            ×
          </button>
        </div>
      ))}
    </div>
  )
}
