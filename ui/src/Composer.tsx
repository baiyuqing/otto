import { useState, type KeyboardEvent } from 'react'

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

  const submit = () => {
    const t = text.trim()
    if (!t || props.disabled || props.running || props.compacting) return
    setText('')
    props.onSend(t)
  }

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    // Enter sends; Shift+Enter inserts a newline. Ignore Enter while an IME
    // composition is in progress so CJK input can confirm a candidate.
    if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault()
      submit()
    }
  }

  return (
    <div className="composer">
      <textarea
        value={text}
        placeholder={props.disabled ? 'Open a session first' : 'Message otto (Enter to send, Shift+Enter for newline)'}
        disabled={props.disabled || props.running || props.compacting}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={onKeyDown}
      />
      <button
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
        <button onClick={props.onCancel}>Cancel</button>
      ) : (
        <button onClick={submit} disabled={props.disabled || props.compacting || !text.trim()}>
          Send
        </button>
      )}
    </div>
  )
}
