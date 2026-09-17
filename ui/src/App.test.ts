// @vitest-environment jsdom
import { act, cleanup, render, screen } from '@testing-library/react'
import { createElement } from 'react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Info, Session } from './wire'

const api = vi.hoisted(() => ({
  info: vi.fn(),
  listSessions: vi.fn(),
  createSession: vi.fn(),
  getSession: vi.fn(),
  history: vi.fn(),
  attach: vi.fn(),
  listTasks: vi.fn(),
}))

vi.mock('./api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./api')>()
  return {
    ...actual,
    api,
    loadToken: () => '',
    setToken: () => {},
  }
})

import { App } from './App'
import { IDLE_POLL_MS } from './follow'

const sandbox = { mode: 'seatbelt', network: 'off', bash_available: true, summary: 'seatbelt' }

const idle: Session = {
  id: 'sess1',
  name: 'dev',
  workspace: '/tmp/otto-work',
  provider: 'openai-compatible',
  profile: 'default',
  model: 'test',
  context_window: 128000,
  usage: { input_tokens: 0, output_tokens: 0 },
  context_input_tokens: 0,
  sandbox,
  turn: null,
}

const info: Info = {
  workspace: '/tmp/otto-work',
  provider: 'openai-compatible',
  profile: 'default',
  model: 'test',
  sandbox: 'seatbelt',
  profiles: ['default'],
}

const emptyHistory = '[]'
const wakeHistory = JSON.stringify([
  {
    id: '1',
    role: 'context',
    created_at: '',
    display: true,
    blocks: [{ type: 'text', text: '[feishu] p2p oc_chat from ou_user\nhello' }],
  },
  { id: '2', role: 'assistant', created_at: '', blocks: [{ type: 'text', text: 'Hi there.' }] },
])

describe('idle wake follow', () => {
  beforeEach(() => {
    vi.useFakeTimers()
    location.hash = '#sess1'
    api.info.mockResolvedValue(info)
    api.listSessions.mockResolvedValue({ sessions: [{ id: 'sess1', name: 'dev', open: true }] })
    api.createSession.mockResolvedValue(idle)
    api.getSession.mockResolvedValue(idle)
    api.history.mockResolvedValue(emptyHistory)
    api.listTasks.mockResolvedValue({ tasks: [] })
    api.attach.mockResolvedValue(new Response('', { headers: { 'Content-Type': 'text/event-stream' } }))
  })

  afterEach(() => {
    cleanup()
    vi.useRealTimers()
    vi.clearAllMocks()
    location.hash = ''
  })

  async function openIdleSession() {
    render(createElement(App))
    for (let i = 0; i < 20; i++) {
      await act(async () => {
        await Promise.resolve()
      })
      if (screen.queryAllByText('dev').length > 0) return
    }
    throw new Error('session did not open')
  }

  it('attaches to a running server-started turn', async () => {
    await openIdleSession()
    expect(api.attach).not.toHaveBeenCalled()

    const running: Session = { ...idle, turn: { id: 'wake1', trigger: 'task', status: 'running' } }
    const done: Session = {
      ...running,
      turn: { id: 'wake1', trigger: 'task', status: 'ok' },
      context_input_tokens: 40,
      usage: { input_tokens: 10, output_tokens: 8 },
    }
    api.getSession.mockImplementation(async () => (api.attach.mock.calls.length > 0 ? done : running))

    await act(async () => {
      await vi.advanceTimersByTimeAsync(IDLE_POLL_MS)
    })

    expect(api.attach).toHaveBeenCalledWith('sess1', 'wake1')
  })

  it('reloads history when a wake finished between polls', async () => {
    await openIdleSession()
    api.history.mockResolvedValue(wakeHistory)
    api.getSession.mockResolvedValue({
      ...idle,
      turn: { id: 'wake1', trigger: 'task', status: 'ok' },
      context_input_tokens: 40,
      usage: { input_tokens: 10, output_tokens: 8 },
    })

    await act(async () => {
      await vi.advanceTimersByTimeAsync(IDLE_POLL_MS)
    })

    expect(api.attach).not.toHaveBeenCalled()
    expect(screen.getByText(/\[feishu\].*hello/s)).toBeTruthy()
    expect(screen.getByText('Hi there.')).toBeTruthy()
  })
})
