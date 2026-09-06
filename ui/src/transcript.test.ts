import { describe, expect, it } from 'vitest'
import { fromHistory, reduce, type Item } from './transcript'
import { parseFrames } from './sse'
import type { Message, WireEvent } from './types'

const apply = (events: WireEvent[], start: Item[] = []) => events.reduce(reduce, start)

describe('reduce', () => {
  it('merges text deltas into one assistant item', () => {
    const items = apply([
      { type: 'agent_started' },
      { type: 'text_delta', text: 'hel' },
      { type: 'text_delta', text: 'lo' },
      { type: 'agent_finished' },
    ])
    expect(items).toEqual([{ kind: 'assistant', text: 'hello' }])
  })

  it('starts a new assistant item after a tool call', () => {
    const items = apply([
      { type: 'text_delta', text: 'first' },
      { type: 'tool_call_started', tool_call_id: 'c1', tool_name: 'bash', tool_args: { command: 'ls' } },
      { type: 'tool_call_finished', tool_call_id: 'c1', tool_name: 'bash', result: { content: 'a\nb', is_error: false } },
      { type: 'text_delta', text: 'second' },
    ])
    expect(items).toEqual([
      { kind: 'assistant', text: 'first' },
      { kind: 'tool', id: 'c1', name: 'bash', args: '{\n  "command": "ls"\n}', result: 'a\nb', isError: false },
      { kind: 'assistant', text: 'second' },
    ])
  })

  it('records an unmatched tool_call_finished as its own item', () => {
    const items = apply([{ type: 'tool_call_finished', tool_call_id: 'c9', tool_name: 'read', result: { content: 'x', is_error: true } }])
    expect(items).toEqual([{ kind: 'tool', id: 'c9', name: 'read', args: '', result: 'x', isError: true }])
  })

  it('does not mutate the previous transcript', () => {
    const before: Item[] = [{ kind: 'assistant', text: 'a' }]
    const after = reduce(before, { type: 'text_delta', text: 'b' })
    expect(before).toEqual([{ kind: 'assistant', text: 'a' }])
    expect(after).toEqual([{ kind: 'assistant', text: 'ab' }])
  })

  it('adds a notice for a real compaction and nothing for a noop', () => {
    const done: WireEvent = {
      type: 'compaction_completed',
      compaction: { reason: 'threshold', tokens_before: 900, estimated_tokens_after: 300, automatic: true, noop: false },
    }
    expect(apply([done])).toEqual([{ kind: 'notice', text: 'Context compacted: 900 → ~300 tokens (threshold)' }])
    const noop: WireEvent = { type: 'compaction_completed', compaction: { ...done.compaction!, noop: true } }
    expect(apply([noop])).toEqual([])
  })

  it('turns agent_error into an error item', () => {
    expect(apply([{ type: 'agent_error', error: 'provider: 500' }])).toEqual([{ kind: 'error', text: 'provider: 500' }])
  })
})

describe('fromHistory', () => {
  const messages: Message[] = [
    { id: '1', role: 'user', created_at: '', blocks: [{ type: 'text', text: 'list files' }] },
    {
      id: '2',
      role: 'assistant',
      created_at: '',
      blocks: [
        { type: 'text', text: 'Running ls.' },
        { type: 'tool_call', tool_call_id: 'c1', tool_name: 'bash', arguments: { command: 'ls' } },
      ],
    },
    { id: '3', role: 'tool', created_at: '', blocks: [{ type: 'tool_result', tool_call_id: 'c1', tool_name: 'bash', text: 'a.go', is_error: false }] },
    { id: '4', role: 'context', created_at: '', context_type: 'memory', blocks: [{ type: 'text', text: 'hidden recall' }] },
    { id: '5', role: 'context', created_at: '', display: true, blocks: [{ type: 'text', text: 'Task t1 finished' }] },
    { id: '6', role: 'assistant', created_at: '', blocks: [{ type: 'text', text: 'One file.' }] },
  ]

  it('pairs tool results and shows only display context', () => {
    expect(fromHistory(messages)).toEqual([
      { kind: 'user', text: 'list files' },
      { kind: 'assistant', text: 'Running ls.' },
      { kind: 'tool', id: 'c1', name: 'bash', args: '{\n  "command": "ls"\n}', result: 'a.go', isError: false },
      { kind: 'notice', text: 'Task t1 finished' },
      { kind: 'assistant', text: 'One file.' },
    ])
  })
})

describe('parseFrames', () => {
  it('splits complete frames and keeps a partial one', () => {
    const { frames, rest } = parseFrames('id: 0\nevent: text_delta\ndata: {"a":1}\n\nid: 1\nevent: agent_fin')
    expect(frames).toEqual([{ id: 0, event: 'text_delta', data: '{"a":1}' }])
    expect(rest).toBe('id: 1\nevent: agent_fin')
  })
})
