import { useState, type KeyboardEvent } from 'react'
import { webCommandSuggestions } from './commands'
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
  const suggestions = props.disabled || props.running || props.compacting ? [] : webCommandSuggestions(text)

  const completeSuggestion = (command: string) => {
    setText(command + ' ')
  }

  const submit = () => {
    const t = text.trim()
    if (!t || props.disabled || props.running || props.compacting) return
    setText('')
    props.onSend(t)
  }

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Tab' && suggestions.length > 0) {
      e.preventDefault()
      completeSuggestion(suggestions[0].name)
      return
    }
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
        {suggestions.length > 0 && (
          <div className="command-suggestions" role="listbox" aria-label="Slash command suggestions">
            {suggestions.map((suggestion) => (
              <button key={suggestion.name} type="button" role="option" onClick={() => completeSuggestion(suggestion.name)}>
                <code>{suggestion.name}</code>
                <span>{suggestion.description}</span>
              </button>
            ))}
          </div>
        )}
        <textarea
          value={text}
          placeholder={props.disabled ? 'Open or create a session first' : 'Ask Otto to inspect, edit, or verify this workspace…'}
          disabled={props.disabled || props.running || props.compacting}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={onKeyDown}
        />
        <div className="composer-actions">
          <span className="composer-hint">
            {props.running
              ? 'Otto is working…'
              : props.compacting
                ? 'Compacting context…'
                : suggestions.length > 0
                  ? 'Tab or click to complete a command'
                  : hint}
          </span>
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
