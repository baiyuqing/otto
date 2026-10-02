// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, within } from '@testing-library/react'
import { createElement } from 'react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Info, Session } from './wire'

const api = vi.hoisted(() => ({
  info: vi.fn(),
  usage: vi.fn(),
  usageDaily: vi.fn(),
  listSessions: vi.fn(),
  createSession: vi.fn(),
  getSession: vi.fn(),
  history: vi.fn(),
  attach: vi.fn(),
  renameSession: vi.fn(),
  listTasks: vi.fn(),
  listMcp: vi.fn(),
  cancelTurn: vi.fn(),
  startTurn: vi.fn(),
  decideApproval: vi.fn(),
  compact: vi.fn(),
  listWorkspaces: vi.fn(),
  addWorkspace: vi.fn(),
  getWorkspaceDiff: vi.fn(),
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
  thinking: 'high',
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
  thinking: 'high',
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
    api.usage.mockResolvedValue({
      requests: 0,
      reported_requests: 0,
      input_tokens: 0,
      output_tokens: 0,
      cached_input_tokens: 0,
      cache_hit_rate: 0,
    })
    api.usageDaily.mockResolvedValue({
      summary: {
        requests: 0,
        reported_requests: 0,
        input_tokens: 0,
        output_tokens: 0,
        cached_input_tokens: 0,
        cache_hit_rate: 0,
      },
      daily: [],
    })
    api.listSessions.mockResolvedValue({ sessions: [{ id: 'sess1', name: 'dev', open: true }] })
    api.createSession.mockResolvedValue(idle)
    api.getSession.mockResolvedValue(idle)
    api.history.mockResolvedValue(emptyHistory)
    api.listTasks.mockResolvedValue({ tasks: [] })
    api.listMcp.mockResolvedValue({ servers: [] })
    api.attach.mockResolvedValue(new Response('', { headers: { 'Content-Type': 'text/event-stream' } }))
    api.cancelTurn.mockResolvedValue(new Response(null, { status: 204 }))
    api.startTurn.mockResolvedValue(new Response('', { headers: { 'Content-Type': 'text/event-stream' } }))
    api.compact.mockResolvedValue({ noop: true })
    api.listWorkspaces.mockResolvedValue({ startup: '/tmp/otto-work', roots: [], workspaces: [{ path: '/tmp/otto-work', open_sessions: 1, workflows: true }] })
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
    // History stops before the turn, whose events replay from sequence 0.
    expect(api.history).toHaveBeenLastCalledWith('sess1', 'wake1')
  })

  it('cancels a running turn when Escape is pressed', async () => {
    const running: Session = { ...idle, turn: { id: 'turn1', trigger: 'user', status: 'running' } }
    api.createSession.mockResolvedValue(running)
    api.attach.mockResolvedValue(new Response(new ReadableStream(), { headers: { 'Content-Type': 'text/event-stream' } }))
    await openIdleSession()
    expect(api.history).toHaveBeenCalledWith('sess1', 'turn1')

    await act(async () => {
      await Promise.resolve()
    })
    fireEvent.keyDown(window, { key: 'Escape' })

    expect(api.cancelTurn).toHaveBeenCalledWith('sess1', 'turn1')
  })

  it('queues input into the transcript on Enter while a turn is running and sends it after success', async () => {
    const running: Session = { ...idle, turn: { id: 'turn1', trigger: 'user', status: 'running' } }
    const done: Session = { ...idle, turn: { id: 'turn1', trigger: 'user', status: 'ok' } }
    const nextRunning: Session = { ...idle, turn: { id: 'turn2', trigger: 'user', status: 'running' } }
    let closeStream = () => {}
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        closeStream = () => controller.close()
      },
    })
    api.createSession.mockResolvedValue(running)
    api.attach.mockResolvedValue(new Response(stream, { headers: { 'Content-Type': 'text/event-stream' } }))
    await openIdleSession()

    const input = screen.getByPlaceholderText('Queue next input…') as HTMLTextAreaElement
    fireEvent.change(input, { target: { value: 'follow up' } })
    fireEvent.keyDown(input, { key: 'Enter' })

    expect(input.value).toBe('')
    expect(within(document.querySelector('.transcript') as HTMLElement).getByText('follow up')).toBeTruthy()
    expect(screen.getAllByText(/Queued next input/).length).toBeGreaterThan(0)
    expect(api.startTurn).not.toHaveBeenCalled()
    api.getSession.mockResolvedValueOnce(done).mockResolvedValue(nextRunning)
    api.startTurn.mockResolvedValue(
      new Response(new ReadableStream(), { headers: { 'Content-Type': 'text/event-stream', 'Otto-Turn-Id': 'turn2' } }),
    )
    closeStream()

    await act(async () => {
      await Promise.resolve()
      await Promise.resolve()
    })

    expect(api.startTurn).toHaveBeenCalledWith('sess1', 'follow up', undefined)
  })

  it('withdraws queued input while a turn is running', async () => {
    const running: Session = { ...idle, turn: { id: 'turn1', trigger: 'user', status: 'running' } }
    api.createSession.mockResolvedValue(running)
    api.attach.mockResolvedValue(new Response(new ReadableStream(), { headers: { 'Content-Type': 'text/event-stream' } }))
    await openIdleSession()

    const input = screen.getByPlaceholderText('Queue next input…') as HTMLTextAreaElement
    fireEvent.change(input, { target: { value: 'follow up' } })
    fireEvent.keyDown(input, { key: 'Enter' })
    expect(input.value).toBe('')
    expect(within(document.querySelector('.transcript') as HTMLElement).getByText('follow up')).toBeTruthy()

    fireEvent.click(screen.getByRole('button', { name: 'Withdraw queued input' }))

    expect(within(document.querySelector('.transcript') as HTMLElement).queryByText('follow up')).toBeNull()
    expect(api.startTurn).not.toHaveBeenCalled()
  })

  it('sends with queue, reads Otto-Turn-Id, shows queued until the first frame and takes the prompt from user_message', async () => {
    let push = (_frame: string) => {}
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        push = (frame) => controller.enqueue(new TextEncoder().encode(frame))
      },
    })
    api.startTurn.mockResolvedValue(
      new Response(stream, { headers: { 'Content-Type': 'text/event-stream', 'Otto-Turn-Id': 'turn9' } }),
    )
    await openIdleSession()
    const transcript = () => document.querySelector('.transcript') as HTMLElement

    const input = screen.getByPlaceholderText('Message Otto…') as HTMLTextAreaElement
    fireEvent.change(input, { target: { value: 'hello' } })
    fireEvent.keyDown(input, { key: 'Enter' })
    await act(async () => {
      await Promise.resolve()
      await Promise.resolve()
    })

    expect(api.startTurn).toHaveBeenCalledWith('sess1', 'hello', undefined)
    expect(document.querySelector('.footer')?.textContent).toContain('queued')
    expect(within(transcript()).queryByText('hello')).toBeNull()
    fireEvent.keyDown(window, { key: 'Escape' })
    expect(api.cancelTurn).toHaveBeenCalledWith('sess1', 'turn9')

    await act(async () => {
      push('id: 0\nevent: user_message\ndata: {"type":"user_message","text":"hello"}\n\n')
      for (let i = 0; i < 20; i++) await Promise.resolve()
    })

    expect(document.querySelector('.footer')?.textContent).not.toContain('queued')
    expect(within(transcript()).getAllByText('hello')).toHaveLength(1)
  })

  it('posts /approve and /deny to the running turn without starting a turn', async () => {
    const running: Session = { ...idle, turn: { id: 'turn1', trigger: 'user', status: 'running' } }
    api.createSession.mockResolvedValue(running)
    api.attach.mockResolvedValue(new Response(new ReadableStream(), { headers: { 'Content-Type': 'text/event-stream' } }))
    api.decideApproval.mockResolvedValue({ decision: 'allow' })
    await openIdleSession()

    const input = screen.getByPlaceholderText('Queue next input…') as HTMLTextAreaElement
    for (const text of ['/approve a1', '/deny a2']) {
      fireEvent.change(input, { target: { value: text } })
      fireEvent.keyDown(input, { key: 'Enter' })
      await act(async () => {
        await Promise.resolve()
      })
    }

    expect(api.decideApproval).toHaveBeenNthCalledWith(1, 'sess1', 'a1', 'allow')
    expect(api.decideApproval).toHaveBeenNthCalledWith(2, 'sess1', 'a2', 'deny')
    expect(api.startTurn).not.toHaveBeenCalled()
    expect(screen.queryByText(/Queued next input/)).toBeNull()
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

  it('switches from chat to the usage analysis page', async () => {
    await openIdleSession()

    fireEvent.click(screen.getByRole('button', { name: 'Usage' }))

    expect(screen.getByRole('heading', { name: 'Usage' })).toBeTruthy()
    await act(async () => {
      await Promise.resolve()
    })
    expect(api.usageDaily).toHaveBeenCalledWith(30)
  })

  it('shows the Otto logo in the header', async () => {
    await openIdleSession()

    const mark = document.querySelector('.brand-mark')
    expect(mark?.tagName).toBe('IMG')
    expect(mark?.getAttribute('alt')).toBe('')
  })

  it('renames the active session from a custom dialog', async () => {
    await openIdleSession()
    api.renameSession.mockResolvedValue({ ...idle, name: 'polished ui' })
    const promptSpy = vi.spyOn(window, 'prompt')

    fireEvent.click(screen.getByRole('button', { name: 'Rename' }))

    const dialog = screen.getByRole('dialog', { name: 'Rename session' })
    expect(dialog).toBeTruthy()
    const input = screen.getByLabelText('Session name') as HTMLInputElement
    expect(input.value).toBe('dev')
    expect(promptSpy).not.toHaveBeenCalled()

    fireEvent.change(input, { target: { value: ' polished ui ' } })
    fireEvent.click(screen.getByRole('button', { name: 'Save name' }))

    await act(async () => {
      await Promise.resolve()
    })

    expect(api.renameSession).toHaveBeenCalledWith('sess1', 'polished ui')
    expect(screen.queryByRole('dialog', { name: 'Rename session' })).toBeNull()
    promptSpy.mockRestore()
  })

  it('lists MCP servers for the /mcp command', async () => {
    await openIdleSession()
    api.listMcp.mockResolvedValue({
      servers: [{ name: 'docs', transport: 'http', protocol_version: '2026-07-28', state: 'connected', tools: 3, error: null }],
    })

    const input = screen.getByPlaceholderText('Message Otto…')
    fireEvent.change(input, { target: { value: '/mcp' } })
    fireEvent.keyDown(input, { key: 'Enter' })
    await act(async () => {
      await Promise.resolve()
    })

    expect(api.listMcp).toHaveBeenCalledWith('sess1')
    expect(screen.getByText('docs: connected (3 tools) (http, 2026-07-28)')).toBeTruthy()
  })
})

describe('Changes view', () => {
  beforeEach(() => {
    api.info.mockResolvedValue(info)
    api.usage.mockResolvedValue({
      requests: 0,
      reported_requests: 0,
      input_tokens: 0,
      output_tokens: 0,
      cached_input_tokens: 0,
      cache_hit_rate: 0,
    })
    api.listSessions.mockResolvedValue({ sessions: [] })
    api.listWorkspaces.mockResolvedValue({ startup: '/tmp/otto-work', roots: [], workspaces: [{ path: '/tmp/otto-work', open_sessions: 0, workflows: true }] })
    api.getWorkspaceDiff.mockResolvedValue({ workspace: '/tmp/otto-work', repository: true, branch: 'main', files: [], truncated: false })
  })

  afterEach(() => {
    cleanup()
    vi.clearAllMocks()
  })

  it('opens the Changes view for a group when its Changes button is clicked', async () => {
    render(createElement(App))
    const header = await screen.findByTitle('/tmp/otto-work')
    fireEvent.click(within(header).getByRole('button', { name: 'Changes' }))

    expect(await screen.findByRole('heading', { name: 'Changes' })).toBeTruthy()
    expect(await screen.findByText('main')).toBeTruthy()
    expect(api.getWorkspaceDiff).toHaveBeenCalledWith('/tmp/otto-work')
  })
})
