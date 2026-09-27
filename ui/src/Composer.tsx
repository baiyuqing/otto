import { useEffect, useLayoutEffect, useRef, useState, type ClipboardEvent, type KeyboardEvent } from 'react'
import { webCommandSuggestions } from './commands'
import { sendHint } from './uiText'

export function Composer(props: {
  disabled: boolean
  running: boolean
  compacting: boolean
  queuedText?: string
  onSend: (text: string, image?: { data: string; mime_type: string }) => void
  onQueue: (text: string) => void
  onWithdrawQueue: () => void
  onCancel: () => void
  // onCompact receives the composer text as the optional focus.
  onCompact: (focus: string) => void
}) {
  const [text, setText] = useState(props.queuedText ?? '')
  const [image, setImage] = useState<{ name: string; data: string; mime_type: string } | null>(null)
  const textareaRef = useRef<HTMLTextAreaElement>(null)
  const hint = sendHint()
  const busy = props.running || props.compacting
  const queueing = props.running
  const queued = queueing && text.trim().length > 0
  const suggestions = props.disabled || busy ? [] : webCommandSuggestions(text)

  useEffect(() => {
    if (props.running) setText(props.queuedText ?? '')
  }, [props.running, props.queuedText])

  useLayoutEffect(() => {
    const textarea = textareaRef.current
    if (!textarea) return
    textarea.style.height = 'auto'
    textarea.style.height = `${textarea.scrollHeight}px`
  }, [text])

  const completeSuggestion = (command: string) => {
    setText(command + ' ')
  }

  const changeText = (next: string) => {
    setText(next)
    if (queueing) props.onQueue(next)
  }

  const withdraw = () => {
    if (!queueing) return
    setText('')
    props.onWithdrawQueue()
  }

  const submit = () => {
    const t = text.trim()
    if (queueing) return
    if ((!t && !image) || props.disabled || props.compacting) return
    setText('')
    const selected = image ? { data: image.data, mime_type: image.mime_type } : undefined
    setImage(null)
    props.onSend(t, selected)
  }

  const attach = (file?: File) => {
    if (busy || !file || !['image/png', 'image/jpeg', 'image/webp'].includes(file.type)) return
    const reader = new FileReader()
    reader.onload = () => {
      const encoded = String(reader.result).split(',', 2)[1]
      if (encoded) setImage({ name: file.name, data: encoded, mime_type: file.type })
    }
    reader.readAsDataURL(file)
  }

  const onPaste = (e: ClipboardEvent<HTMLTextAreaElement>) => {
    if (busy) return
    const item = Array.from(e.clipboardData.items).find((candidate) => candidate.type.startsWith('image/'))
    const file = item?.getAsFile() ?? Array.from(e.clipboardData.files).find((candidate) => candidate.type.startsWith('image/'))
    if (file) attach(file)
  }

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (queueing && e.key.toLowerCase() === 'u' && (e.ctrlKey || e.metaKey)) {
      e.preventDefault()
      withdraw()
      return
    }
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

  const placeholder = props.disabled ? 'Open or create a session first' : queueing ? 'Queue next input…' : 'Message Otto…'
  const hintText = queueing
    ? queued
      ? 'Queued next input · edit below · Ctrl+U withdraw · Esc cancels turn'
      : 'Otto is working · type to queue next input · Esc cancels turn'
    : props.compacting
      ? 'Compacting context…'
      : suggestions.length > 0
        ? 'Tab or click to complete a command'
        : hint

  return (
    <section className={`composer${queued ? ' queued' : queueing ? ' running' : ''}`} aria-label="Message composer">
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
        {queueing && (
          <div className="queued-input-label" aria-live="polite">
            <strong>{queued ? 'Queued next input' : 'Working'}</strong>
            <span>{queued ? 'Editable until the current turn finishes.' : 'Type below to queue the next input.'}</span>
          </div>
        )}
        <textarea
          ref={textareaRef}
          value={text}
          placeholder={placeholder}
          disabled={props.disabled || props.compacting}
          onChange={(e) => changeText(e.target.value)}
          onKeyDown={onKeyDown}
          onPaste={onPaste}
        />
        {image && (
          <div className="image-attachment">
            <img src={`data:${image.mime_type};base64,${image.data}`} alt={image.name} />
            <span>{image.name}</span>
            <button type="button" aria-label="Remove image" onClick={() => setImage(null)}>
              ×
            </button>
          </div>
        )}
        <div className="composer-actions">
          <span className="composer-hint">{hintText}</span>
          <label className={`image-picker${props.disabled || busy ? ' disabled' : ''}`}>
            Image
            <input
              aria-label="Attach image"
              type="file"
              accept="image/png,image/jpeg,image/webp"
              disabled={props.disabled || busy}
              onChange={(e) => attach(e.target.files?.[0])}
            />
          </label>
          <button
            className="secondary"
            title="Compact the context; the text above, if any, is the focus"
            disabled={props.disabled || busy}
            onClick={() => {
              props.onCompact(text.trim())
              setText('')
            }}
          >
            {props.compacting ? 'Compacting…' : 'Compact'}
          </button>
          {props.running ? (
            <>
              <button className="secondary" disabled={!queued} onClick={withdraw} aria-label="Withdraw queued input">
                Withdraw
              </button>
              <button className="danger" onClick={props.onCancel}>
                Cancel turn
              </button>
            </>
          ) : (
            <button className="primary" onClick={submit} disabled={props.disabled || props.compacting || (!text.trim() && !image)}>
              Send
            </button>
          )}
        </div>
      </div>
    </section>
  )
}
