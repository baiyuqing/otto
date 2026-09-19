import { useState, type ClipboardEvent, type KeyboardEvent } from 'react'
import { webCommandSuggestions } from './commands'
import { sendHint } from './uiText'

export function Composer(props: {
  disabled: boolean
  running: boolean
  compacting: boolean
  onSend: (text: string, image?: { data: string; mime_type: string }) => void
  onCancel: () => void
  // onCompact receives the composer text as the optional focus.
  onCompact: (focus: string) => void
}) {
  const [text, setText] = useState('')
  const [image, setImage] = useState<{ name: string; data: string; mime_type: string } | null>(null)
  const hint = sendHint()
  const suggestions = props.disabled || props.running || props.compacting ? [] : webCommandSuggestions(text)

  const completeSuggestion = (command: string) => {
    setText(command + ' ')
  }

  const submit = () => {
    const t = text.trim()
    if (!t || props.disabled || props.running || props.compacting) return
    setText('')
    const selected = image ? { data: image.data, mime_type: image.mime_type } : undefined
    setImage(null)
    props.onSend(t, selected)
  }

  const attach = (file?: File) => {
    if (!file || !['image/png', 'image/jpeg', 'image/webp'].includes(file.type)) return
    const reader = new FileReader()
    reader.onload = () => {
      const encoded = String(reader.result).split(',', 2)[1]
      if (encoded) setImage({ name: file.name, data: encoded, mime_type: file.type })
    }
    reader.readAsDataURL(file)
  }

  const onPaste = (e: ClipboardEvent<HTMLTextAreaElement>) => {
    const file = Array.from(e.clipboardData.files).find((candidate) => candidate.type.startsWith('image/'))
    if (file) attach(file)
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
          placeholder={props.disabled ? 'Open or create a session first' : 'Message Otto…'}
          disabled={props.disabled || props.running || props.compacting}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={onKeyDown}
          onPaste={onPaste}
        />
        {image && (
          <div className="image-attachment">
            <span>{image.name}</span>
            <button type="button" aria-label="Remove image" onClick={() => setImage(null)}>
              ×
            </button>
          </div>
        )}
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
          <label className={`image-picker${props.disabled || props.running || props.compacting ? ' disabled' : ''}`}>
            Image
            <input
              aria-label="Attach image"
              type="file"
              accept="image/png,image/jpeg,image/webp"
              disabled={props.disabled || props.running || props.compacting}
              onChange={(e) => attach(e.target.files?.[0])}
            />
          </label>
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
