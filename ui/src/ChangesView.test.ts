// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { createElement } from 'react'
import { afterEach, describe, expect, it, vi } from 'vitest'

const api = vi.hoisted(() => ({
  getWorkspaceDiff: vi.fn(),
}))

vi.mock('./api', () => ({ api }))

import { ChangesView } from './ChangesView'
import type { SessionStatus, WorkspaceDiff } from './api'

const diff: WorkspaceDiff = {
  workspace: '/Users/me/src/app',
  repository: true,
  branch: 'main',
  files: [
    { path: 'src/a.rs', old_path: null, status: 'modified', binary: false, patch: '@@ -1,3 +1,4 @@\n-old\n+new\n context', truncated: false },
    { path: 'src/renamed.rs', old_path: 'src/old-name.rs', status: 'renamed', binary: false, patch: '', truncated: false },
    { path: 'image.png', old_path: null, status: 'modified', binary: true, patch: '', truncated: false },
    { path: 'src/big.rs', old_path: null, status: 'modified', binary: false, patch: '', truncated: true },
  ],
  truncated: true,
}

describe('ChangesView', () => {
  afterEach(() => {
    cleanup()
    vi.clearAllMocks()
  })

  it('renders files, statuses, a rename, a binary file, and truncation notes', async () => {
    api.getWorkspaceDiff.mockResolvedValue(diff)
    render(createElement(ChangesView, { workspace: '/Users/me/src/app', onError: vi.fn() }))

    expect(await screen.findByText('main')).toBeTruthy()
    expect(api.getWorkspaceDiff).toHaveBeenCalledWith('/Users/me/src/app')

    expect(screen.getByText('src/a.rs')).toBeTruthy()
    expect(screen.getByText('+new')).toBeTruthy()
    expect(screen.getByText('src/old-name.rs → src/renamed.rs')).toBeTruthy()
    expect(screen.getByText('Binary file')).toBeTruthy()
    expect(screen.getAllByText(/truncated/i).length).toBeGreaterThanOrEqual(2)
  })

  it('shows "Not a git repository" when the directory is not a work tree', async () => {
    api.getWorkspaceDiff.mockResolvedValue({ workspace: '/tmp/x', repository: false, branch: null, files: [], truncated: false })
    render(createElement(ChangesView, { workspace: '/tmp/x', onError: vi.fn() }))

    expect(await screen.findByText('Not a git repository')).toBeTruthy()
  })

  it('refetches on Refresh click', async () => {
    api.getWorkspaceDiff.mockResolvedValue(diff)
    render(createElement(ChangesView, { workspace: '/Users/me/src/app', onError: vi.fn() }))
    await screen.findByText('main')
    expect(api.getWorkspaceDiff).toHaveBeenCalledTimes(1)

    fireEvent.click(screen.getByRole('button', { name: 'Refresh' }))
    await waitFor(() => expect(api.getWorkspaceDiff).toHaveBeenCalledTimes(2))
  })

  it('reports a fetch error through onError', async () => {
    const onError = vi.fn()
    api.getWorkspaceDiff.mockRejectedValue(new Error('workspace not admitted'))
    render(createElement(ChangesView, { workspace: '/Users/me/src/app', onError }))

    await waitFor(() => expect(onError).toHaveBeenCalledWith(expect.objectContaining({ message: 'workspace not admitted' })))
  })

  it('ignores a slower response for a previous directory', async () => {
    let resolveFirst: (value: WorkspaceDiff) => void = () => {}
    api.getWorkspaceDiff
      .mockReturnValueOnce(new Promise<WorkspaceDiff>((resolve) => (resolveFirst = resolve)))
      .mockResolvedValueOnce({ ...diff, workspace: '/b', branch: 'second', files: [] })
    const view = render(createElement(ChangesView, { workspace: '/a', onError: vi.fn() }))
    view.rerender(createElement(ChangesView, { workspace: '/b', onError: vi.fn() }))
    expect(await screen.findByText('second')).toBeTruthy()

    resolveFirst({ ...diff, workspace: '/a', branch: 'first' })
    await new Promise((resolve) => setTimeout(resolve, 0))
    expect(screen.queryByText('first')).toBeNull()
    expect(screen.getByText('second')).toBeTruthy()
  })

  it('refetches when a session in its workspace goes from running to ok', async () => {
    api.getWorkspaceDiff.mockResolvedValue(diff)
    const onError = vi.fn()
    const running = new Map<string, SessionStatus>([
      ['s1', { id: 's1', workspace: '/Users/me/src/app', turn: 'running', approvals: 0, tasks: 0 }],
    ])
    const view = render(createElement(ChangesView, { workspace: '/Users/me/src/app', status: running, onError }))
    await screen.findByText('main')
    expect(api.getWorkspaceDiff).toHaveBeenCalledTimes(1)

    const ok = new Map<string, SessionStatus>([
      ['s1', { id: 's1', workspace: '/Users/me/src/app', turn: 'ok', approvals: 0, tasks: 0 }],
    ])
    view.rerender(createElement(ChangesView, { workspace: '/Users/me/src/app', status: ok, onError }))

    await waitFor(() => expect(api.getWorkspaceDiff).toHaveBeenCalledTimes(2))
  })

  it('does not refetch for a running-to-ok transition in another workspace', async () => {
    api.getWorkspaceDiff.mockResolvedValue(diff)
    const onError = vi.fn()
    const running = new Map<string, SessionStatus>([
      ['s1', { id: 's1', workspace: '/Users/me/src/other', turn: 'running', approvals: 0, tasks: 0 }],
    ])
    const view = render(createElement(ChangesView, { workspace: '/Users/me/src/app', status: running, onError }))
    await screen.findByText('main')
    expect(api.getWorkspaceDiff).toHaveBeenCalledTimes(1)

    const ok = new Map<string, SessionStatus>([
      ['s1', { id: 's1', workspace: '/Users/me/src/other', turn: 'ok', approvals: 0, tasks: 0 }],
    ])
    view.rerender(createElement(ChangesView, { workspace: '/Users/me/src/app', status: ok, onError }))

    await new Promise((resolve) => setTimeout(resolve, 0))
    expect(api.getWorkspaceDiff).toHaveBeenCalledTimes(1)
  })
})
