import { useEffect, useId, useRef, useState, type ComponentPropsWithoutRef, type ReactNode } from 'react'
import Markdown from 'react-markdown'
import rehypeKatex from 'rehype-katex'
import remarkGfm from 'remark-gfm'
import remarkMath from 'remark-math'
import 'katex/dist/katex.min.css'
import type { Item } from './wire'

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
          <h1>{props.activeSession ? 'Session is open' : 'No session'}</h1>
          <p>
            {props.activeSession
              ? 'Type below. Otto works in this workspace.'
              : 'Create a session or resume one from the top bar.'}
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
        a({ children, ...props }) {
          return (
            <a {...props} target="_blank" rel="noopener noreferrer">
              {children}
            </a>
          )
        },
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

const TOOL_PREVIEW_LIMIT = 96

function itemCreatedAt(item: Item): string {
  return (item as Item & { created_at?: string }).created_at ?? ''
}

function formatTimestamp(value: string): string {
  const match = /^\d{4}-\d{2}-\d{2}T(\d{2}:\d{2}:\d{2})/.exec(value)
  return match?.[1] ?? value
}

function Timestamp({ value }: { value: string }) {
  if (!value) return null
  return (
    <time className="item-timestamp" dateTime={value} title={value}>
      {formatTimestamp(value)}
    </time>
  )
}

function firstLine(text: string): string {
  const line = text.split(/\r?\n/, 1)[0]?.trim() ?? ''
  return line.length > TOOL_PREVIEW_LIMIT ? `${line.slice(0, TOOL_PREVIEW_LIMIT)}…` : line
}

function parseToolArgs(args: string): Record<string, unknown> | null {
  if (!args.trim()) return null
  try {
    const parsed = JSON.parse(args) as unknown
    return parsed && typeof parsed === 'object' && !Array.isArray(parsed) ? (parsed as Record<string, unknown>) : null
  } catch {
    return null
  }
}

function stringField(value: unknown): string {
  return typeof value === 'string' ? value : ''
}

function diffSnippet(args: Record<string, unknown>): string {
  const oldText = stringField(args.old_text)
  const newText = stringField(args.new_text)
  if (oldText || newText) return [`- ${firstLine(oldText)}`, `+ ${firstLine(newText)}`].filter(Boolean).join(' ')
  const edits = Array.isArray(args.edits) ? args.edits : []
  const firstEdit = edits.find((edit): edit is Record<string, unknown> => !!edit && typeof edit === 'object' && !Array.isArray(edit))
  return firstEdit ? diffSnippet(firstEdit) : ''
}

function toolArgumentSummary(name: string, args: string): string {
  const parsed = parseToolArgs(args)
  if (!parsed) return firstLine(args)
  const path = stringField(parsed.path)
  if (name === 'bash') return firstLine(stringField(parsed.command))
  if (name === 'ls') return path || '.'
  if (name === 'read') return [path, stringField(parsed.offset), stringField(parsed.limit)].filter(Boolean).join(' ')
  if (name === 'edit' || name === 'write') return [path, diffSnippet(parsed)].filter(Boolean).join(' ')
  if (name === 'grep') return [stringField(parsed.pattern), path || stringField(parsed.glob)].filter(Boolean).join(' ')
  if (name === 'find') return [path, stringField(parsed.pattern)].filter(Boolean).join(' ')
  return firstLine(args)
}

function toolResultSummary(item: Extract<Item, { kind: 'tool' }>): string {
  if (item.result === undefined) return 'Running…'
  if (!item.result.trim()) return item.isError ? 'Failed: no output' : 'Done: no output'
  const more = item.result.split(/\r?\n/).length - 1
  const suffix = more > 0 ? ` (+${more} ${more === 1 ? 'line' : 'lines'})` : ''
  return `${item.isError ? 'Failed' : 'Done'}: ${firstLine(item.result)}${suffix}`
}

function ItemFrame({ item, className, children }: { item: Item; className: string; children: ReactNode }) {
  return (
    <div className={className}>
      <Timestamp value={itemCreatedAt(item)} />
      {children}
    </div>
  )
}

export function MermaidDiagram({ code, label }: { code: string; label?: string }) {
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
  if (!svg)
    return (
      <div
        className="mermaid-diagram pending"
        role={label ? 'img' : undefined}
        aria-label={label}
        tabIndex={label ? 0 : undefined}
      >
        Rendering diagram…
      </div>
    )
  return (
    <div
      className="mermaid-diagram"
      role={label ? 'img' : undefined}
      aria-label={label}
      tabIndex={label ? 0 : undefined}
      dangerouslySetInnerHTML={{ __html: svg }}
    />
  )
}

function ItemView({ item }: { item: Item }) {
  switch (item.kind) {
    case 'user':
      return <ItemFrame item={item} className="item user">{item.text}</ItemFrame>
    case 'image':
      return (
        <ItemFrame item={item} className="item user image">
          <img src={`data:${item.mime_type};base64,${item.data}`} alt="Sent image" />
        </ItemFrame>
      )
    case 'assistant':
      return (
        <ItemFrame item={item} className="item assistant">
          <MarkdownView text={item.text} />
        </ItemFrame>
      )
    case 'reasoning': {
      const [first, ...rest] = item.text.split('\n')
      return (
        <details className="item reasoning">
          <summary>{first}</summary>
          {rest.length > 0 && <pre>{rest.join('\n')}</pre>}
        </details>
      )
    }
    case 'tool':
      return (
        <details className={`item tool${item.isError ? ' error' : ''}`}>
          <summary>
            <span className="tool-heading">
              <Timestamp value={itemCreatedAt(item)} />
              <strong>{item.name} </strong>
              {item.args && <span className="tool-args-preview">{toolArgumentSummary(item.name, item.args)}</span>}
            </span>
            <span className="tool-state"> {toolResultSummary(item)}</span>
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
