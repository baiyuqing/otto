import type { Message, WireEvent } from './types'

export type Item =
  | { kind: 'user'; text: string }
  | { kind: 'assistant'; text: string }
  | { kind: 'tool'; id: string; name: string; args: string; result?: string; isError?: boolean }
  | { kind: 'notice'; text: string }
  | { kind: 'error'; text: string }

function formatArgs(args: unknown): string {
  if (args === undefined || args === null) return ''
  if (typeof args === 'string') return args
  return JSON.stringify(args, null, 2)
}

// fromHistory renders the stored session history. Tool results are matched
// to their tool call by id; context messages appear only when marked
// display (compaction summaries and task notifications the server chose to
// show).
export function fromHistory(messages: Message[]): Item[] {
  const items: Item[] = []
  const tools = new Map<string, Extract<Item, { kind: 'tool' }>>()
  for (const m of messages) {
    for (const b of m.blocks) {
      if (b.type === 'tool_result') {
        const t = b.tool_call_id ? tools.get(b.tool_call_id) : undefined
        if (t) {
          t.result = b.text ?? ''
          t.isError = b.is_error
        }
        continue
      }
      if (b.type === 'tool_call') {
        const t = { kind: 'tool' as const, id: b.tool_call_id ?? '', name: b.tool_name ?? '', args: formatArgs(b.arguments) }
        tools.set(t.id, t)
        items.push(t)
        continue
      }
      if (!b.text) continue
      switch (m.role) {
        case 'user':
          items.push({ kind: 'user', text: b.text })
          break
        case 'assistant':
          items.push({ kind: 'assistant', text: b.text })
          break
        case 'context':
          if (m.display) items.push({ kind: 'notice', text: b.text })
          break
      }
    }
  }
  return items
}

// reduce applies one turn event and returns the next transcript. Items are
// treated as immutable so React re-renders only what changed.
export function reduce(items: Item[], ev: WireEvent): Item[] {
  const last = items[items.length - 1]
  switch (ev.type) {
    case 'text_delta': {
      if (!ev.text) return items
      if (last?.kind === 'assistant') {
        return [...items.slice(0, -1), { kind: 'assistant', text: last.text + ev.text }]
      }
      return [...items, { kind: 'assistant', text: ev.text }]
    }
    case 'tool_call_started':
      return [...items, { kind: 'tool', id: ev.tool_call_id ?? '', name: ev.tool_name ?? '', args: formatArgs(ev.tool_args) }]
    case 'tool_call_finished': {
      const result = ev.result?.content ?? ''
      const isError = ev.result?.is_error ?? false
      const i = items.findLastIndex((it) => it.kind === 'tool' && it.id === ev.tool_call_id)
      if (i < 0) {
        // Seen when attaching mid-turn with ?after=N past the start event.
        return [...items, { kind: 'tool', id: ev.tool_call_id ?? '', name: ev.tool_name ?? '', args: '', result, isError }]
      }
      const t = items[i] as Extract<Item, { kind: 'tool' }>
      return [...items.slice(0, i), { ...t, result, isError }, ...items.slice(i + 1)]
    }
    case 'compaction_completed': {
      const c = ev.compaction
      if (!c || c.noop) return items
      return [...items, { kind: 'notice', text: `Context compacted: ${c.tokens_before} → ~${c.estimated_tokens_after} tokens (${c.reason})` }]
    }
    case 'notification':
      return [...items, { kind: 'notice', text: ev.text ?? `Task ${ev.task_id ?? ''} finished` }]
    case 'compaction_warning':
    case 'memory_warning':
      return [...items, { kind: 'notice', text: ev.text || ev.error || ev.type }]
    case 'agent_error':
      return [...items, { kind: 'error', text: ev.error || ev.text || 'agent error' }]
    default:
      // agent_started, agent_finished, provider_usage, compaction_planned,
      // compaction_started carry nothing the transcript shows.
      return items
  }
}
