// Reader for the server's SSE frames: "id: N\nevent: T\ndata: J\n\n"
// (internal/server/server.go writeSSEFrame). fetch + a hand-written parser
// instead of EventSource, which cannot send the Authorization header and
// reconnects forever once a finished turn's stream closes.

export interface Frame {
  id: number | null
  event: string
  data: string
}

// parseFrames splits buf into complete frames and returns the unparsed
// remainder, so a chunk boundary in the middle of a frame is safe.
export function parseFrames(buf: string): { frames: Frame[]; rest: string } {
  const frames: Frame[] = []
  let start = 0
  for (;;) {
    const end = buf.indexOf('\n\n', start)
    if (end < 0) break
    const frame = parseFrame(buf.slice(start, end))
    if (frame) frames.push(frame)
    start = end + 2
  }
  return { frames, rest: buf.slice(start) }
}

function parseFrame(block: string): Frame | null {
  const frame: Frame = { id: null, event: '', data: '' }
  const data: string[] = []
  for (const line of block.split('\n')) {
    if (line.startsWith('id: ')) frame.id = Number(line.slice(4))
    else if (line.startsWith('event: ')) frame.event = line.slice(7)
    else if (line.startsWith('data: ')) data.push(line.slice(6))
  }
  frame.data = data.join('\n')
  return frame.event ? frame : null
}

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
