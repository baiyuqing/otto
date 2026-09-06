import { useEffect, useRef } from 'react'
import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import type { Item } from './transcript'

export function TranscriptView(props: { items: Item[]; activeSession: boolean }) {
  const ref = useRef<HTMLDivElement>(null)
  // Follow the stream: keep the newest item in view as it grows.
  useEffect(() => {
    const el = ref.current
    if (el) el.scrollTop = el.scrollHeight
  }, [props.items])

  return (
    <div className="transcript" ref={ref}>
      {props.items.length === 0 ? (
        <div className="empty-state">
          <div className="empty-orb">✦</div>
          <h1>{props.activeSession ? 'Ready when you are.' : 'Open a session to start.'}</h1>
          <p>
            {props.activeSession
              ? 'Ask Otto to explore the codebase, make a focused edit, or run verification.'
              : 'Create a new session or resume an existing one from the top bar.'}
          </p>
        </div>
      ) : (
        props.items.map((it, i) => <ItemView key={i} item={it} />)
      )}
    </div>
  )
}

function ItemView({ item }: { item: Item }) {
  switch (item.kind) {
    case 'user':
      return <div className="item user">{item.text}</div>
    case 'assistant':
      return (
        <div className="item assistant">
          <Markdown remarkPlugins={[remarkGfm]}>{item.text}</Markdown>
        </div>
      )
    case 'tool':
      return (
        <details className={`item tool${item.isError ? ' error' : ''}`}>
          <summary>
            <span>{item.name}</span>
            <span>{item.result === undefined ? 'Running…' : item.isError ? 'Failed' : 'Done'}</span>
          </summary>
          {item.args && <pre>{item.args}</pre>}
          {item.result !== undefined && <pre>{item.result}</pre>}
        </details>
      )
    case 'notice':
      return <div className="item notice">{item.text}</div>
    case 'error':
      return <div className="item error">{item.text}</div>
  }
}
