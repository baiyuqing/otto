// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { createElement } from 'react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const api = vi.hoisted(() => ({
  listWorkflows: vi.fn(),
  getWorkflow: vi.fn(),
  startWorkflow: vi.fn(),
  approveWorkflow: vi.fn(),
  rejectWorkflow: vi.fn(),
  resumeWorkflow: vi.fn(),
  cancelWorkflow: vi.fn(),
}))

vi.mock('./api', () => ({ api }))

import { WorkflowsView } from './WorkflowsView'

const run = {
  id: 'run-12345678',
  workflow: 'review',
  workspace: '/tmp/work',
  profile: 'default',
  provider: 'openai-compatible',
  model: 'test',
  input: 'change',
  status: 'waiting' as const,
  steps: [
    {
      id: 'review',
      kind: 'handoff' as const,
      agent: 'reviewer',
      prompt: 'Review',
      needs: ['research'],
      status: 'succeeded' as const,
      attempt: 1,
      result: 'looks good',
      error: '',
      transcript_path: '',
    },
    {
      id: 'approve',
      kind: 'approval' as const,
      agent: '',
      prompt: 'Ship?',
      needs: [],
      status: 'waiting' as const,
      attempt: 0,
      result: '',
      error: '',
      transcript_path: '',
    },
  ],
}

const view = {
  run,
  requests: [{ id: 'request-1', run_id: run.id, step_id: 'approve', prompt: 'Ship?', status: 'pending' as const }],
}

describe('WorkflowsView', () => {
  beforeEach(() => {
    api.listWorkflows.mockResolvedValue({ runs: [run] })
    api.getWorkflow.mockResolvedValue(view)
    api.approveWorkflow.mockResolvedValue({ ...view, requests: [] })
  })

  afterEach(() => {
    cleanup()
    vi.clearAllMocks()
  })

  it('shows durable steps and handles an approval gate', async () => {
    render(createElement(WorkflowsView, { onError: vi.fn() }))
    const runButton = await screen.findByRole('button', { name: /review/ })
    fireEvent.click(runButton)
    expect(await screen.findByText(/handoff to reviewer/)).toBeTruthy()
    expect(await screen.findByText('Approval required · approve')).toBeTruthy()

    fireEvent.click(screen.getByRole('button', { name: 'Approve' }))
    await waitFor(() => expect(api.approveWorkflow).toHaveBeenCalledWith('request-1'))
  })
})
