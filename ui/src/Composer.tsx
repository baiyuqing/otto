import { useState, type KeyboardEvent } from 'react'
import { sendHint } from './uiText'

export function Composer(props: {
  disabled: boolean
  running: boolean
  compacting: boolean
  onSend: (text: string) => void
  onCancel: () => void
  // onCompact receives the composer text as the optional focus.
  onCompact: (focus: string) => void
}) {
  const [text, setText] = useState('')
  const hint = sendHint()

  const submit = () => {
    const t = text.trim()
    if (!t || props.disabled || props.running || props.compacting) return
    setText('')
    props.onSend(t)
  }

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    // Enter sends; Shift+Enter inserts a newline. Cmd/Ctrl+Enter also sends
    // for users who expect editor-style submission. Ignore shortcuts while an
    // IME composition is in progress so CJK input can confirm a candidate.
    if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault()
      submit()
    }
  }

  return (
    <section className="composer" aria-label="Message composer">
      <div className="composer-card">
        <textarea
          value={text}
          placeholder={props.disabled ? 'Open or create a session first' : 'Ask Otto to inspect, edit, or verify this workspace…'}
          disabled={props.disabled || props.running || props.compacting}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={onKeyDown}
        />
        <div className="composer-actions">
          <span className="composer-hint">{props.running ? 'Otto is working…' : props.compacting ? 'Compacting context…' : hint}</span>
          <button
            className="secondary"
            title="Compact the context; the text above, if any, is the focus"
            disabled={props.disabled || props.running || props.compacting}
            onClick={() => {
              props.onCompact(text.trim())
              setText('')
            }}
          >
            {props.compacting ? 'Compacting…' : 'Compact'}
          </button>
          {props.running ? (
            <button className="danger" onClick={props.onCancel}>
              Cancel
            </button>
          ) : (
            <button className="primary" onClick={submit} disabled={props.disabled || props.compacting || !text.trim()}>
              Send
            </button>
          )}
        </div>
      </div>
    </section>
  )
}
