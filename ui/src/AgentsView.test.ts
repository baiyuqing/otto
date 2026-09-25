// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { createElement } from 'react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const api = vi.hoisted(() => ({
  listAgentTasks: vi.fn(),
  getAgentTask: vi.fn(),
  listSessions: vi.fn(),
  cancelTask: vi.fn(),
}))

vi.mock('./api', () => ({ api }))

import { AgentsView } from './AgentsView'
import type { AgentTask, AgentTaskDetail } from './api'

const running: AgentTask = {
  parent_session: '01JRUNNING',
  task_id: 't1',
  workspace: '/Users/me/src/app',
  parent_session_path: '/Users/me/.otto/sessions/x/01JRUNNING.jsonl',
  name: undefined,
  agent: '',
  description: 'review the diff',
  model: 'gpt-5.1',
  context: 'fresh',
  prompt: 'review this',
  status: 'running',
  created_at: '2026-09-25T10:00:00Z',
  started_at: '2026-09-25T10:00:01Z',
  finished_at: '',
  steps: 3,
  tool_calls: 5,
  last_tool: 'read',
  input_tokens: 12000,
  output_tokens: 800,
  cached_tokens: 9000,
  result: '',
  error: '',
  session_path: '/Users/me/.otto/sessions/x/01JRUNNING/t1-01K.jsonl',
  cancelable: true,
}

const done: AgentTask = {
  ...running,
  parent_session: '01JDONE',
  task_id: 't2',
  name: 'reviewer',
  agent: 'code-reviewer',
  description: 'summarize the release notes',
  workspace: '/Users/me/src/other',
  status: 'succeeded',
  finished_at: '2026-09-25T10:00:20Z',
  result: 'looks good',
  cancelable: false,
  parent_session_path: '',
}

const detail: AgentTaskDetail = {
  task: running,
  history: [{ id: '1', role: 'assistant', created_at: '', blocks: [{ type: 'text', text: 'Working on it.' }] }],
  transcript_missing: false,
}

describe('AgentsView', () => {
  beforeEach(() => {
    api.listAgentTasks.mockResolvedValue({ tasks: [running, done], next_before: '2026-09-25T09:00:00Z' })
    api.listSessions.mockResolvedValue({ sessions: [{ id: '01JRUNNING', open: true }] })
    api.getAgentTask.mockResolvedValue(detail)
    api.cancelTask.mockResolvedValue({})
  })

  afterEach(() => {
    cleanup()
    vi.clearAllMocks()
    vi.useRealTimers()
  })

  it('renders rows and applies status and workspace filters', async () => {
    render(createElement(AgentsView, { onError: vi.fn(), onOpenSession: vi.fn() }))

    expect(await screen.findByText('review the diff')).toBeTruthy()
    expect(screen.getByText('default')).toBeTruthy() // running.agent is empty
    expect(screen.getByText('code-reviewer')).toBeTruthy()
    const app = screen.getByTitle('/Users/me/src/app')
    expect(app.textContent).toBe('app')

    fireEvent.change(screen.getByLabelText('Status'), { target: { value: 'running' } })
    await waitFor(() => expect(api.listAgentTasks).toHaveBeenLastCalledWith(expect.objectContaining({ status: 'running' })))

    fireEvent.change(screen.getByLabelText('Workspace filter'), { target: { value: '/Users/me/src/app' } })
    fireEvent.submit(screen.getByLabelText('Workspace filter'))
    await waitFor(() =>
      expect(api.listAgentTasks).toHaveBeenLastCalledWith(expect.objectContaining({ workspace: '/Users/me/src/app' })),
    )
  })

  it('loads the next page on Load more', async () => {
    render(createElement(AgentsView, { onError: vi.fn(), onOpenSession: vi.fn() }))
    await screen.findByText('review the diff')

    fireEvent.click(screen.getByRole('button', { name: 'Load more' }))
    await waitFor(() =>
      expect(api.listAgentTasks).toHaveBeenLastCalledWith(expect.objectContaining({ before: '2026-09-25T09:00:00Z' })),
    )
  })

  it('polls only while a visible row is queued or running', async () => {
    vi.useFakeTimers()
    render(createElement(AgentsView, { onError: vi.fn(), onOpenSession: vi.fn() }))
    // Flush the mocked fetch promises (pure microtasks; fake timers do not
    // advance the clock for these) so the polling interval is registered at
    // fake time 0, before any advance below.
    for (let i = 0; i < 10; i++) {
      await act(async () => {
        await Promise.resolve()
      })
    }
    expect(api.listAgentTasks).toHaveBeenCalledTimes(1)

    await act(async () => {
      await vi.advanceTimersByTimeAsync(3100)
    })
    expect(api.listAgentTasks).toHaveBeenCalledTimes(2)

    api.listAgentTasks.mockResolvedValue({ tasks: [done], next_before: '' })
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3100)
    })
    expect(api.listAgentTasks).toHaveBeenCalledTimes(3)

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1000)
    })
    expect(api.listAgentTasks).toHaveBeenCalledTimes(3)
  })

  it('selecting a row shows the prompt, result, transcript, and Cancel only when cancelable', async () => {
    render(createElement(AgentsView, { onError: vi.fn(), onOpenSession: vi.fn() }))
    await screen.findByText('review the diff')

    fireEvent.click(screen.getByText('review the diff'))
    await waitFor(() => expect(api.getAgentTask).toHaveBeenCalledWith('01JRUNNING', 't1'))
    expect(await screen.findByText('review this')).toBeTruthy()
    expect(await screen.findByText('Working on it.')).toBeTruthy()
    expect(screen.getByRole('button', { name: 'Cancel' })).toBeTruthy()

    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }))
    await waitFor(() => expect(api.cancelTask).toHaveBeenCalledWith('01JRUNNING', 't1'))
  })

  it('opens the parent session in Chat when the server owns it, otherwise shows the path', async () => {
    const onOpenSession = vi.fn()
    render(createElement(AgentsView, { onError: vi.fn(), onOpenSession }))
    await screen.findByText('review the diff')

    fireEvent.click(screen.getByText('review the diff'))
    const openButton = await screen.findByRole('button', { name: /open session/i })
    fireEvent.click(openButton)
    expect(onOpenSession).toHaveBeenCalledWith('01JRUNNING')

    api.getAgentTask.mockResolvedValueOnce({ ...detail, task: done, transcript_missing: true })
    fireEvent.click(screen.getByText('code-reviewer'))
    await waitFor(() => expect(api.getAgentTask).toHaveBeenCalledWith('01JDONE', 't2'))
    expect(screen.queryByRole('button', { name: /open session/i })).toBeNull()
    expect(await screen.findByText(/transcript/i)).toBeTruthy()
  })
})
