// @vitest-environment jsdom
import { createElement } from 'react'
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { Approvals } from './Approvals'
import { api, type ApprovalRequest } from './api'
vi.mock('./api', () => ({ api: { approvals: vi.fn(), sharedApproval: vi.fn(), decideSharedApproval: vi.fn() } }))
const request: ApprovalRequest = { id: 'request-1', revision: 3, kind: 'confirmation', title: 'Review the parser plan', reason: 'Confirm the scope', persistent: false, blocking: true, subject: { source: 'conversation', id: 'op-1', version: 'v1', details: 'Change only parsing' }, state: 'pending', application: 'not_required', application_error: '' }
beforeEach(() => {
  vi.clearAllMocks()
  vi.mocked(api.approvals).mockResolvedValue({ requests: [request], cursor: 1 })
  vi.mocked(api.decideSharedApproval).mockResolvedValue({ ...request, state: 'rejected' })
})
afterEach(cleanup)
it('follows application failure after a committed approval without claiming success', async () => {
  vi.mocked(api.decideSharedApproval).mockResolvedValue({ ...request, state: 'approved', application: 'applying' })
  vi.mocked(api.sharedApproval).mockResolvedValue({ ...request, state: 'approved', application: 'failed', application_error: 'Target changed before dispatch' })
  render(createElement(Approvals, { sessionId: 'session-1' }))
  fireEvent.click(await screen.findByText('Approve'))
  await waitFor(() => expect(screen.getByRole('status').textContent).toContain('application: failed'))
  expect(screen.getByRole('status').textContent).toContain('Target changed before dispatch')
  expect(api.sharedApproval).toHaveBeenCalledWith('session-1', 'request-1')
})
it('binds feedback and rejection to the displayed request version', async () => {
  render(createElement(Approvals, { sessionId: "session-1" }))
  await screen.findByText(request.title)
  fireEvent.change(screen.getByLabelText('Feedback'), { target: { value: 'Keep the old syntax' } })
  fireEvent.click(screen.getByText('Reject'))
  await waitFor(() => expect(api.decideSharedApproval).toHaveBeenCalledWith('session-1', 'request-1', 3, 'reject', 'Keep the old syntax'))
  expect(await screen.findByRole('status')).toHaveProperty('textContent', `${request.title}: rejected`)
})
it('shows a queue lookup failure instead of an empty queue', async () => {
  vi.mocked(api.approvals).mockRejectedValue(new Error('Approval service unavailable'))
  render(createElement(Approvals, { sessionId: "session-1" }))
  expect(await screen.findByRole('alert')).toHaveProperty('textContent', 'Approval service unavailable')
})
it('keeps a stale decision actionable as an explicit error', async () => {
  vi.mocked(api.decideSharedApproval).mockRejectedValue(new Error('Already decided elsewhere'))
  render(createElement(Approvals, { sessionId: "session-1" }))
  fireEvent.click(await screen.findByText('Approve'))
  expect(await screen.findByRole('alert')).toHaveProperty('textContent', 'Already decided elsewhere')
  expect(screen.getByText('Approve')).toBeDefined()
})
