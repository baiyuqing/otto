// The wire format and the transcript reducer live in Rust
// (crates/otto-core/src/wire) and reach the browser through the wasm package
// `make ui` builds into crates/otto-web/pkg. This module re-exports them and
// adds the one piece that cannot be wasm: the ReadableStream adapter.

export type {
  Block,
  Compaction,
  Frame,
  Info,
  Message,
  ParsedFrames,
  Sandbox,
  Session,
  SessionListRow,
  SessionTurn,
  Task,
  TaskDetail,
  TurnStatus,
  TurnSummary,
  Usage,
  WireEvent,
} from 'otto-web'

import type { Item as WebItem } from 'otto-web'

export type Item =
  | (Extract<WebItem, { kind: 'user' }> & { created_at?: string })
  | (Extract<WebItem, { kind: 'image' }> & { created_at?: string })
  | (Extract<WebItem, { kind: 'assistant' }> & { created_at?: string })
  | (Extract<WebItem, { kind: 'reasoning' }> & { created_at?: string })
  | (Extract<WebItem, { kind: 'tool' }> & { created_at?: string })
  | Extract<WebItem, { kind: 'notice' | 'error' }>

export { fromHistory, parseFrames, reduce } from 'otto-web'

import init, { parseFrames, type Frame } from 'otto-web'

// ready resolves once the wasm module is instantiated. Every export above
// traps if it is called first, so the entry point awaits this before render.
export const ready: Promise<unknown> = init()

// readSSE turns a fetch body into a stream of frames. Reading the stream is
// browser plumbing, so it stays in TypeScript; the framing itself is
// parseFrames from the wasm package.
export async function* readSSE(body: ReadableStream<Uint8Array>): AsyncGenerator<Frame> {
  const reader = body.getReader()
  const decoder = new TextDecoder()
  let buf = ''
  try {
    for (;;) {
      const { value, done } = await reader.read()
      if (done) return
      buf += decoder.decode(value, { stream: true })
      const parsed = parseFrames(buf)
      buf = parsed.rest
      for (const frame of parsed.frames) yield frame
    }
  } finally {
    reader.releaseLock()
  }
}
