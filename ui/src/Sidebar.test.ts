// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { createElement } from 'react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const api = vi.hoisted(() => ({
  listWorkspaces: vi.fn(),
  addWorkspace: vi.fn(),
  removeWorkspace: vi.fn(),
  listDirs: vi.fn(),
}))

vi.mock('./api', async (importOriginal) => ({ ...(await importOriginal<typeof import('./api')>()), api }))

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

  it('adds a group on a successful typed-path add', async () => {
    api.addWorkspace.mockResolvedValue({ path: '/Users/me/src/new', open_sessions: 0, workflows: true })
    render(createElement(Sidebar, { sessions, current: '', disabled: false, onOpen: vi.fn() }))
    await screen.findByText('other-session')

    fireEvent.change(screen.getByLabelText('Add workspace path'), { target: { value: '/Users/me/src/new' } })
    fireEvent.click(screen.getByRole('button', { name: 'Add path' }))

    await waitFor(() => expect(screen.getByTitle('/Users/me/src/new')).toBeTruthy())
    expect(api.addWorkspace).toHaveBeenCalledWith('/Users/me/src/new', false)
  })

  it('shows the server error inline when adding a workspace is rejected', async () => {
    const { ApiError } = await vi.importActual<typeof import('./api')>('./api')
    api.addWorkspace.mockRejectedValue(new ApiError(400, 'INVALID_WORKSPACE', '/nope: not an existing directory'))
    render(createElement(Sidebar, { sessions, current: '', disabled: false, onOpen: vi.fn() }))
    await screen.findByText('other-session')

    fireEvent.change(screen.getByLabelText('Add workspace path'), { target: { value: '/nope' } })
    fireEvent.click(screen.getByRole('button', { name: 'Add path' }))

    expect(await screen.findByText('/nope: not an existing directory')).toBeTruthy()
  })

  it('asks to trust a folder that is not admitted yet, then adds it with trust', async () => {
    const { ApiError } = await vi.importActual<typeof import('./api')>('./api')
    api.addWorkspace
      .mockRejectedValueOnce(new ApiError(403, 'WORKSPACE_NOT_ADMITTED', 'not admitted'))
      .mockResolvedValueOnce({ path: '/Users/me/src/new', open_sessions: 0, workflows: true })
    render(createElement(Sidebar, { sessions, current: '', disabled: false, onOpen: vi.fn() }))
    await screen.findByText('other-session')

    fireEvent.change(screen.getByLabelText('Add workspace path'), { target: { value: '/Users/me/src/new' } })
    fireEvent.click(screen.getByRole('button', { name: 'Add path' }))
    const dialog = await screen.findByRole('dialog', { name: 'Trust this folder?' })
    expect(within(dialog).getByText('/Users/me/src/new')).toBeTruthy()
    fireEvent.click(within(dialog).getByRole('button', { name: 'Trust and add' }))

    await waitFor(() => expect(screen.getByTitle('/Users/me/src/new')).toBeTruthy())
    expect(api.addWorkspace).toHaveBeenLastCalledWith('/Users/me/src/new', true)
    expect(screen.queryByRole('dialog')).toBeNull()
  })

  it('adds nothing when the trust question is cancelled', async () => {
    const { ApiError } = await vi.importActual<typeof import('./api')>('./api')
    api.addWorkspace.mockRejectedValueOnce(new ApiError(403, 'WORKSPACE_NOT_ADMITTED', 'not admitted'))
    render(createElement(Sidebar, { sessions, current: '', disabled: false, onOpen: vi.fn() }))
    await screen.findByText('other-session')

    fireEvent.change(screen.getByLabelText('Add workspace path'), { target: { value: '/Users/me/src/new' } })
    fireEvent.click(screen.getByRole('button', { name: 'Add path' }))
    const dialog = await screen.findByRole('dialog', { name: 'Trust this folder?' })
    fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }))

    expect(screen.queryByRole('dialog')).toBeNull()
    expect(api.addWorkspace).toHaveBeenCalledTimes(1)
    expect(screen.queryByTitle('/Users/me/src/new')).toBeNull()
  })

  it('in a browser, picks a folder by browsing the server and adds it', async () => {
    api.listDirs
      .mockResolvedValueOnce({
        path: '/Users/me',
        parent: null,
        roots: ['/Users/me'],
        dirs: [{ name: 'src', path: '/Users/me/src' }],
      })
      .mockResolvedValueOnce({
        path: '/Users/me/src',
        parent: '/Users/me',
        roots: ['/Users/me'],
        dirs: [{ name: 'new', path: '/Users/me/src/new' }],
      })
    api.addWorkspace.mockResolvedValue({ path: '/Users/me/src', open_sessions: 0, workflows: true })
    render(createElement(Sidebar, { sessions, current: '', disabled: false, onOpen: vi.fn() }))
    await screen.findByText('other-session')

    fireEvent.click(screen.getByRole('button', { name: 'Add workspace…' }))
    const picker = await screen.findByRole('dialog', { name: 'Choose a folder' })
    fireEvent.click(await within(picker).findByRole('button', { name: 'src' }))
    await within(picker).findByRole('button', { name: 'new' })
    expect(api.listDirs).toHaveBeenLastCalledWith('/Users/me/src')
    fireEvent.click(within(picker).getByRole('button', { name: 'Choose this folder' }))

    await waitFor(() => expect(api.addWorkspace).toHaveBeenCalledWith('/Users/me/src', false))
    await waitFor(() => expect(screen.queryByRole('dialog', { name: 'Choose a folder' })).toBeNull())
  })

  it('goes up to the parent folder in the picker', async () => {
    api.listDirs
      .mockResolvedValueOnce({ path: '/Users/me/src', parent: '/Users/me', roots: ['/Users/me'], dirs: [] })
      .mockResolvedValueOnce({ path: '/Users/me', parent: null, roots: ['/Users/me'], dirs: [] })
    render(createElement(Sidebar, { sessions, current: '', disabled: false, onOpen: vi.fn() }))
    await screen.findByText('other-session')

    fireEvent.click(screen.getByRole('button', { name: 'Add workspace…' }))
    const picker = await screen.findByRole('dialog', { name: 'Choose a folder' })
    const up = await within(picker).findByRole('button', { name: 'Up' })
    await waitFor(() => expect((up as HTMLButtonElement).disabled).toBe(false))
    fireEvent.click(up)

    await waitFor(() => expect(api.listDirs).toHaveBeenLastCalledWith('/Users/me'))
    await waitFor(() => expect((within(picker).getByRole('button', { name: 'Up' }) as HTMLButtonElement).disabled).toBe(true))
  })

  describe('in the desktop app', () => {
    const openFolder = vi.fn()
    beforeEach(() => {
      ;(window as unknown as { __OTTO_DESKTOP__?: unknown }).__OTTO_DESKTOP__ = { openFolder }
    })
    afterEach(() => {
      delete (window as unknown as { __OTTO_DESKTOP__?: unknown }).__OTTO_DESKTOP__
      openFolder.mockReset()
    })

    it('hands Add workspace to the app, which picks, trusts, and registers the folder', async () => {
      render(createElement(Sidebar, { sessions, current: '', disabled: false, onOpen: vi.fn() }))
      await screen.findByText('other-session')

      fireEvent.click(screen.getByRole('button', { name: 'Add workspace…' }))

      expect(openFolder).toHaveBeenCalledTimes(1)
      expect(api.addWorkspace).not.toHaveBeenCalled()
      expect(api.listDirs).not.toHaveBeenCalled()
      expect(screen.queryByRole('dialog')).toBeNull()
    })
  })

  it('disables every row and control while disabled', async () => {
    render(createElement(Sidebar, { sessions, current: '', disabled: true, onOpen: vi.fn() }))
    await screen.findByText('other-session')

    for (const button of screen.getAllByRole('button')) expect((button as HTMLButtonElement).disabled).toBe(true)
    expect((screen.getByLabelText('Add workspace path') as HTMLInputElement).disabled).toBe(true)
  })

  it('shows a Remove button for non-startup groups only', async () => {
    render(createElement(Sidebar, { sessions, current: '', disabled: false, onOpen: vi.fn() }))
    await waitFor(() => expect(screen.getAllByRole('button', { name: 'Remove' })).toHaveLength(1))

    const appHeader = screen.getByTitle('/Users/me/src/app')
    expect(within(appHeader).getByRole('button', { name: 'Remove' })).toBeTruthy()
    const otherHeader = screen.getByTitle('/Users/me/src/other')
    expect(within(otherHeader).queryByRole('button', { name: 'Remove' })).toBeNull()
  })

  it('calls the API and onWorkspaceRemoved on a successful workspace remove', async () => {
    api.removeWorkspace.mockResolvedValue(undefined)
    const onWorkspaceRemoved = vi.fn()
    // A session-less group so a successful remove also drops it from the
    // sidebar (a group with sessions stays, since groupSessions unions
    // workspaces with the workspaces sessions report).
    api.listWorkspaces.mockResolvedValue({
      startup: '/Users/me/src/other',
      roots: [],
      workspaces: [...workspaces, { path: '/Users/me/src/empty', open_sessions: 0, workflows: false }],
    })
    render(createElement(Sidebar, { sessions, current: '', disabled: false, onOpen: vi.fn(), onWorkspaceRemoved }))
    const emptyHeader = await screen.findByTitle('/Users/me/src/empty')

    fireEvent.click(within(emptyHeader).getByRole('button', { name: 'Remove' }))

    expect(api.removeWorkspace).toHaveBeenCalledWith('/Users/me/src/empty')
    await waitFor(() => expect(screen.queryByTitle('/Users/me/src/empty')).toBeNull())
    expect(onWorkspaceRemoved).toHaveBeenCalledTimes(1)
  })

  it('shows the server error inline when removing a workspace is rejected', async () => {
    const { ApiError } = await vi.importActual<typeof import('./api')>('./api')
    api.removeWorkspace.mockRejectedValue(new ApiError(409, 'WORKSPACE_IN_USE', 'workspace in use'))
    render(createElement(Sidebar, { sessions, current: '', disabled: false, onOpen: vi.fn() }))
    const appHeader = await screen.findByTitle('/Users/me/src/app')

    fireEvent.click(within(appHeader).getByRole('button', { name: 'Remove' }))

    expect(await screen.findByText('workspace in use')).toBeTruthy()
    expect(screen.getByTitle('/Users/me/src/app')).toBeTruthy()
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
