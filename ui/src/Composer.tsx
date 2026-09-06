import { useState, type KeyboardEvent } from 'react'

export function Composer(props: {
  disabled: boolean
  running: boolean
  onSend: (text: string) => void
  onCancel: () => void
}) {
  const [text, setText] = useState('')

  const submit = () => {
    const t = text.trim()
    if (!t || props.disabled || props.running) return
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
        disabled={props.disabled || props.running}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={onKeyDown}
      />
      {props.running ? (
        <button onClick={props.onCancel}>Cancel</button>
      ) : (
        <button onClick={submit} disabled={props.disabled || !text.trim()}>
          Send
        </button>
      )}
    </div>
  )
}
