// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { createElement } from 'react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const api = vi.hoisted(() => ({
  listWorkspaces: vi.fn(),
  addWorkspace: vi.fn(),
}))

vi.mock('./api', () => ({ api }))

import { SessionPicker } from './SessionPicker'
import type { SessionListRow } from './wire'

const workspaceList = {
  startup: '/Users/me/src/app',
  roots: [] as string[],
  workspaces: [
    { path: '/Users/me/src/app', open_sessions: 1, workflows: true },
    { path: '/Users/me/src/other', open_sessions: 0, workflows: true },
  ],
}

describe('SessionPicker', () => {
  beforeEach(() => {
    api.listWorkspaces.mockResolvedValue(workspaceList)
  })

  afterEach(() => {
    cleanup()
    vi.clearAllMocks()
  })

  it('shows the startup workspace first and selected by default', async () => {
    render(createElement(SessionPicker, { sessions: [], current: '', disabled: false, onOpen: vi.fn() }))

    const select = await screen.findByLabelText('Workspace')
    await waitFor(() => expect(select.querySelectorAll('option')).toHaveLength(2))
    expect(select.querySelectorAll('option')[0].getAttribute('value')).toBe('/Users/me/src/app')
    expect((select as HTMLSelectElement).value).toBe('/Users/me/src/app')
  })

  it('adds a workspace and selects it', async () => {
    api.addWorkspace.mockResolvedValue({ path: '/Users/me/src/new', open_sessions: 0, workflows: true })
    render(createElement(SessionPicker, { sessions: [], current: '', disabled: false, onOpen: vi.fn() }))
    await screen.findByLabelText('Workspace')

    fireEvent.change(screen.getByLabelText('Add workspace path'), { target: { value: '/Users/me/src/new' } })
    fireEvent.click(screen.getByRole('button', { name: 'Add workspace' }))

    await waitFor(() => expect(api.addWorkspace).toHaveBeenCalledWith('/Users/me/src/new'))
    const select = (await screen.findByLabelText('Workspace')) as HTMLSelectElement
    await waitFor(() => expect(select.value).toBe('/Users/me/src/new'))
  })

  it('shows the server error inline when adding a workspace is rejected', async () => {
    const { ApiError } = await vi.importActual<typeof import('./api')>('./api')
    api.addWorkspace.mockRejectedValue(new ApiError(403, 'WORKSPACE_NOT_ADMITTED', 'workspace not admitted'))
    render(createElement(SessionPicker, { sessions: [], current: '', disabled: false, onOpen: vi.fn() }))
    await screen.findByLabelText('Workspace')

    fireEvent.change(screen.getByLabelText('Add workspace path'), { target: { value: '/etc' } })
    fireEvent.click(screen.getByRole('button', { name: 'Add workspace' }))

    expect(await screen.findByText('workspace not admitted')).toBeTruthy()
  })

  it('sends the workspace only when a non-startup workspace is selected', async () => {
    const onOpen = vi.fn()
    render(createElement(SessionPicker, { sessions: [], current: '', disabled: false, onOpen }))
    await screen.findByLabelText('Workspace')

    fireEvent.click(screen.getByRole('button', { name: 'New session' }))
    expect(onOpen).toHaveBeenLastCalledWith(undefined, undefined)

    fireEvent.change(screen.getByLabelText('Workspace'), { target: { value: '/Users/me/src/other' } })
    fireEvent.click(screen.getByRole('button', { name: 'New session' }))
    expect(onOpen).toHaveBeenLastCalledWith(undefined, '/Users/me/src/other')
  })

  it('shows a session workspace basename next to its name, unchanged when absent', async () => {
    const sessions: SessionListRow[] = [
      { id: '1', name: undefined, open: false, model: 'gpt-5.1', workspace: '/Users/me/src/app' } as SessionListRow,
      { id: '2', name: undefined, open: false, model: 'gpt-5.1' } as SessionListRow,
    ]
    render(createElement(SessionPicker, { sessions, current: '', disabled: false, onOpen: vi.fn() }))

    const withWorkspace = screen.getByTitle('/Users/me/src/app')
    expect(withWorkspace.textContent).toContain('app')

    const sessionSelect = screen.getByLabelText('Open session')
    const withoutWorkspace = sessionSelect.querySelector('option[value="2"]')
    expect(withoutWorkspace?.getAttribute('title')).toBeNull()
  })
})
