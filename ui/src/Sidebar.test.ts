// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { createElement } from 'react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const api = vi.hoisted(() => ({
  listWorkspaces: vi.fn(),
  addWorkspace: vi.fn(),
}))

vi.mock('./api', () => ({ api }))

import { groupSessions, Sidebar } from './Sidebar'
import type { SessionListRow } from './wire'
import type { SessionStatus } from './api'

const workspaces = [
  { path: '/Users/me/src/app', open_sessions: 2, workflows: true },
  { path: '/Users/me/src/other', open_sessions: 0, workflows: true },
]

const workspaceList = { startup: '/Users/me/src/other', roots: [] as string[], workspaces }

describe('groupSessions', () => {
  it('unions workspace entries and session workspaces, startup first then sorted, keeping empty groups and server row order', () => {
    const sessions: SessionListRow[] = [
      { id: '2', open: true, workspace: '/Users/me/src/app' } as SessionListRow,
      { id: '1', open: false, workspace: '/Users/me/src/app' } as SessionListRow,
      { id: '3', open: false, workspace: '/Users/me/src/extra' } as SessionListRow,
    ]

    const groups = groupSessions('/Users/me/src/other', workspaces, sessions)

    expect(groups.map((g) => g.path)).toEqual(['/Users/me/src/other', '/Users/me/src/app', '/Users/me/src/extra'])
    expect(groups[0].sessions).toEqual([])
    expect(groups[1].sessions.map((s) => s.id)).toEqual(['2', '1'])
    expect(groups[2].sessions.map((s) => s.id)).toEqual(['3'])
  })
})

describe('Sidebar', () => {
  beforeEach(() => {
    api.listWorkspaces.mockResolvedValue(workspaceList)
  })

  afterEach(() => {
    cleanup()
    vi.clearAllMocks()
  })

  const sessions: SessionListRow[] = [
    { id: '1', name: undefined, open: true, model: 'gpt-5.1', workspace: '/Users/me/src/app' } as SessionListRow,
    { id: '2', name: 'other-session', open: false, model: 'gpt-5.1', workspace: '/Users/me/src/other' } as SessionListRow,
  ]

  it('calls onOpen(id) when a session row is clicked', async () => {
    const onOpen = vi.fn()
    render(createElement(Sidebar, { sessions, current: '', disabled: false, onOpen }))
    await screen.findByText('other-session')

    fireEvent.click(screen.getByText('other-session'))
    expect(onOpen).toHaveBeenCalledWith('2')
  })

  it('marks the current session row with aria-current', async () => {
    render(createElement(Sidebar, { sessions, current: '2', disabled: false, onOpen: vi.fn() }))
    await screen.findByText('other-session')

    expect(screen.getByText('other-session').closest('button')?.getAttribute('aria-current')).toBe('true')
    const otherRow = screen.getAllByRole('button', { name: /gpt-5.1/ }).find((b) => !b.textContent?.includes('other-session'))
    expect(otherRow?.getAttribute('aria-current')).toBeNull()
  })

  it('calls onOpen(undefined, path) for a non-startup group and onOpen(undefined, undefined) for the startup group', async () => {
    const onOpen = vi.fn()
    render(createElement(Sidebar, { sessions, current: '', disabled: false, onOpen }))
    await waitFor(() => expect(screen.getAllByRole('button', { name: 'New session' })).toHaveLength(2))

    const appHeader = screen.getByTitle('/Users/me/src/app')
    fireEvent.click(within(appHeader).getByRole('button', { name: 'New session' }))
    expect(onOpen).toHaveBeenLastCalledWith(undefined, '/Users/me/src/app')

    const otherHeader = screen.getByTitle('/Users/me/src/other')
    fireEvent.click(within(otherHeader).getByRole('button', { name: 'New session' }))
    expect(onOpen).toHaveBeenLastCalledWith(undefined, undefined)
  })

  it('calls onOpenChanges(path) when a group header Changes button is clicked', async () => {
    const onOpenChanges = vi.fn()
    render(createElement(Sidebar, { sessions, current: '', disabled: false, onOpen: vi.fn(), onOpenChanges }))
    await waitFor(() => expect(screen.getAllByRole('button', { name: 'Changes' })).toHaveLength(2))

    const appHeader = screen.getByTitle('/Users/me/src/app')
    fireEvent.click(within(appHeader).getByRole('button', { name: 'Changes' }))
    expect(onOpenChanges).toHaveBeenCalledWith('/Users/me/src/app')
  })

  it('adds a group on a successful workspace add', async () => {
    api.addWorkspace.mockResolvedValue({ path: '/Users/me/src/new', open_sessions: 0, workflows: true })
    render(createElement(Sidebar, { sessions, current: '', disabled: false, onOpen: vi.fn() }))
    await screen.findByText('other-session')

    fireEvent.change(screen.getByLabelText('Add workspace path'), { target: { value: '/Users/me/src/new' } })
    fireEvent.click(screen.getByRole('button', { name: 'Add workspace' }))

    await waitFor(() => expect(screen.getByTitle('/Users/me/src/new')).toBeTruthy())
  })

  it('shows the server error inline when adding a workspace is rejected', async () => {
    const { ApiError } = await vi.importActual<typeof import('./api')>('./api')
    api.addWorkspace.mockRejectedValue(new ApiError(403, 'WORKSPACE_NOT_ADMITTED', 'workspace not admitted'))
    render(createElement(Sidebar, { sessions, current: '', disabled: false, onOpen: vi.fn() }))
    await screen.findByText('other-session')

    fireEvent.change(screen.getByLabelText('Add workspace path'), { target: { value: '/etc' } })
    fireEvent.click(screen.getByRole('button', { name: 'Add workspace' }))

    expect(await screen.findByText('workspace not admitted')).toBeTruthy()
  })

  it('disables every row and control while disabled', async () => {
    render(createElement(Sidebar, { sessions, current: '', disabled: true, onOpen: vi.fn() }))
    await screen.findByText('other-session')

    for (const button of screen.getAllByRole('button')) expect((button as HTMLButtonElement).disabled).toBe(true)
    expect((screen.getByLabelText('Add workspace path') as HTMLInputElement).disabled).toBe(true)
  })
})

describe('Sidebar status badges', () => {
  beforeEach(() => {
    api.listWorkspaces.mockResolvedValue(workspaceList)
  })

  afterEach(() => {
    cleanup()
    vi.clearAllMocks()
  })

  const statusSessions: SessionListRow[] = [
    { id: '1', name: 'alpha', open: true, model: 'gpt-5.1', workspace: '/Users/me/src/app' } as SessionListRow,
    { id: '2', name: 'beta', open: true, model: 'gpt-5.1', workspace: '/Users/me/src/app' } as SessionListRow,
    { id: '3', name: 'gamma', open: false, model: 'gpt-5.1', workspace: '/Users/me/src/app' } as SessionListRow,
  ]

  it('shows a badge per status field, and no badge for a session without status', async () => {
    const status = new Map<string, SessionStatus>([
      ['1', { id: '1', workspace: '/Users/me/src/app', turn: 'running', approvals: 0, tasks: 0 }],
      ['2', { id: '2', workspace: '/Users/me/src/app', turn: 'error', approvals: 2, tasks: 3 }],
    ])
    render(createElement(Sidebar, { sessions: statusSessions, current: '', disabled: false, onOpen: vi.fn(), status }))
    await screen.findByText('alpha')

    const alphaRow = screen.getByText('alpha').closest('button') as HTMLElement
    expect(within(alphaRow).getByLabelText('Turn running').textContent).toBe('running')

    const betaRow = screen.getByText('beta').closest('button') as HTMLElement
    expect(within(betaRow).getByLabelText('Turn failed').textContent).toBe('error')
    expect(within(betaRow).getByLabelText('Approval pending').textContent).toBe('approval')
    expect(within(betaRow).getByLabelText('3 sub-agent tasks running').textContent).toBe('3 tasks')

    const gammaRow = screen.getByText('gamma').closest('button') as HTMLElement
    expect(within(gammaRow).queryByLabelText(/Turn running|Turn failed|Approval pending|sub-agent tasks running/)).toBeNull()
  })

  it('shows the running count in the group header when greater than zero', async () => {
    const status = new Map<string, SessionStatus>([
      ['1', { id: '1', workspace: '/Users/me/src/app', turn: 'running', approvals: 0, tasks: 0 }],
      ['2', { id: '2', workspace: '/Users/me/src/app', turn: 'running', approvals: 0, tasks: 0 }],
    ])
    render(createElement(Sidebar, { sessions: statusSessions, current: '', disabled: false, onOpen: vi.fn(), status }))

    const appHeader = await screen.findByTitle('/Users/me/src/app')
    expect(within(appHeader).getByText('2 running')).toBeTruthy()

    const otherHeader = screen.getByTitle('/Users/me/src/other')
    expect(within(otherHeader).queryByText(/running/)).toBeNull()
  })
})
