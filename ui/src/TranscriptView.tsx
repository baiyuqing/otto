import { useEffect, useId, useRef, useState, type ComponentPropsWithoutRef } from 'react'
import Markdown from 'react-markdown'
import rehypeKatex from 'rehype-katex'
import remarkGfm from 'remark-gfm'
import remarkMath from 'remark-math'
import 'katex/dist/katex.min.css'
import type { Item } from './transcript'

type MermaidAPI = typeof import('mermaid').default

let mermaidPromise: Promise<MermaidAPI> | null = null

function loadMermaid(): Promise<MermaidAPI> {
  mermaidPromise ??= import('mermaid').then(({ default: mermaid }) => {
    mermaid.initialize({ startOnLoad: false, securityLevel: 'strict', theme: 'default' })
    return mermaid
  })
  return mermaidPromise
}

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

type CodeProps = ComponentPropsWithoutRef<'code'> & { inline?: boolean }

function MarkdownView({ text }: { text: string }) {
  return (
    <Markdown
      remarkPlugins={[remarkGfm, remarkMath]}
      rehypePlugins={[rehypeKatex]}
      components={{
        code({ inline, className, children, ...props }: CodeProps) {
          const match = /language-(\w+)/.exec(className ?? '')
          const code = String(children).replace(/\n$/, '')
          if (!inline && match?.[1] === 'mermaid') return <MermaidDiagram code={code} />
          return (
            <code className={className} {...props}>
              {children}
            </code>
          )
        },
      }}
    >
      {text}
    </Markdown>
  )
}

function MermaidDiagram({ code }: { code: string }) {
  const reactId = useId()
  const id = `mermaid-${reactId.replace(/[^a-zA-Z0-9_-]/g, '')}`
  const [svg, setSVG] = useState('')
  const [error, setError] = useState('')

  useEffect(() => {
    let canceled = false
    setSVG('')
    setError('')
    loadMermaid()
      .then((mermaid) => mermaid.render(id, code))
      .then(({ svg }) => {
        if (!canceled) setSVG(svg)
      })
      .catch((e: unknown) => {
        if (!canceled) setError(e instanceof Error ? e.message : String(e))
      })
    return () => {
      canceled = true
    }
  }, [code, id])

  if (error) {
    return (
      <pre className="mermaid-error">
        <code>{code}</code>
      </pre>
    )
  }
  if (!svg) return <div className="mermaid-diagram pending">Rendering diagram…</div>
  return <div className="mermaid-diagram" dangerouslySetInnerHTML={{ __html: svg }} />
}

function ItemView({ item }: { item: Item }) {
  switch (item.kind) {
    case 'user':
      return <div className="item user">{item.text}</div>
    case 'assistant':
      return (
        <div className="item assistant">
          <MarkdownView text={item.text} />
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
