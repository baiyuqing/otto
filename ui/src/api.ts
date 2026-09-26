import { readSSE, type Compaction, type ContextReport, type Frame, type Info, type Message, type Session, type SessionListRow, type Task, type TaskDetail, type TurnSummary, type WireEvent } from './wire'

export interface UsageSummary {
  requests: number
  reported_requests: number
  input_tokens: number
  output_tokens: number
  cached_input_tokens: number
  cache_hit_rate: number
}

export interface DailyUsage {
  date: string
  requests: number
  reported_requests: number
  input_tokens: number
  output_tokens: number
  cached_input_tokens: number
}

export interface UsageAnalysis {
  summary: UsageSummary
  daily: DailyUsage[]
}

export interface McpServer {
  name: string
  transport: string
  protocol_version: string | null
  state: string
  tools: number
  error: string | null
}

export interface WorkflowStep {
  id: string
  kind: 'agent' | 'approval' | 'handoff'
  agent: string
  prompt: string
  needs: string[]
  status: 'pending' | 'ready' | 'running' | 'waiting' | 'succeeded' | 'failed' | 'canceled' | 'interrupted'
  attempt: number
  result: string
  error: string
  transcript_path: string
  source_run_id: string | null
  source_step_id: string | null
  source_attempt: number | null
}

export interface WorkflowRun {
  id: string
  workflow: string
  workspace: string
  profile: string
  provider: string
  model: string
  input: string
  forked_from_run_id: string | null
  forked_from_event_seq: number | null
  forked_from_step_id: string | null
  status: 'running' | 'waiting' | 'paused' | 'succeeded' | 'failed' | 'canceled'
  steps: WorkflowStep[]
}

export interface WorkflowRequest {
  id: string
  run_id: string
  step_id: string
  prompt: string
  status: 'pending' | 'approved' | 'rejected' | 'canceled'
}

export interface WorkflowView {
  run: WorkflowRun
  requests: WorkflowRequest[]
}

// AgentTask is one row of GET /v1/tasks: a sub-agent run from any session of
// any otto process on the machine, read from ~/.otto/tasks.db.
export interface AgentTask {
  parent_session: string
  task_id: string
  workspace: string
  parent_session_path: string
  name?: string
  agent: string
  description: string
  model?: string
  context?: string
  prompt: string
  status: 'queued' | 'running' | 'succeeded' | 'failed' | 'canceled' | 'interrupted'
  created_at: string
  started_at: string
  finished_at: string
  steps: number
  tool_calls: number
  last_tool?: string
  input_tokens: number
  output_tokens: number
  cached_tokens: number
  result: string
  error: string
  session_path: string
  cancelable: boolean
}

export interface AgentTaskList {
  tasks: AgentTask[]
  next_before: string
}

export interface AgentTaskDetail {
  task: AgentTask
  history: Message[]
  transcript_missing: boolean
}

// WorkspaceEntry is one loaded workspace from GET /v1/workspaces, also the
// body of POST /v1/workspaces's response.
export interface WorkspaceEntry {
  path: string
  open_sessions: number
  workflows: boolean
}

export interface WorkspaceList {
  startup: string
  roots: string[]
  workspaces: WorkspaceEntry[]
}

const TOKEN_KEY = 'otto.token'

// loadToken takes the token from the startup URL's query string, keeps it
// in sessionStorage (per tab, gone when the tab closes), and removes it from
// the address bar so it is not copied along with the URL.
export function loadToken(): string {
  const url = new URL(location.href)
  const fromURL = url.searchParams.get('token')
  if (fromURL) {
    sessionStorage.setItem(TOKEN_KEY, fromURL)
    url.searchParams.delete('token')
    history.replaceState(null, '', url.pathname + url.search + url.hash)
    return fromURL
  }
  return sessionStorage.getItem(TOKEN_KEY) ?? ''
}

let token = ''
export function setToken(t: string) {
  token = t
}

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message)
  }
}

async function request(path: string, init: RequestInit = {}): Promise<Response> {
  const headers = new Headers(init.headers)
  if (token) headers.set('Authorization', `Bearer ${token}`)
  if (init.body) headers.set('Content-Type', 'application/json')
  const res = await fetch(path, { ...init, headers })
  if (!res.ok) {
    let code = 'http_error'
    let message = `${res.status} ${res.statusText}`
    try {
      const body = await res.json()
      code = body.error?.code ?? code
      message = body.error?.message ?? message
    } catch {
      // non-JSON error body; keep the status text
    }
    throw new ApiError(res.status, code, message)
  }
  return res
}

const json = async <T>(path: string, init?: RequestInit): Promise<T> => (await request(path, init)).json()

const text = async (path: string, init?: RequestInit): Promise<string> => (await request(path, init)).text()

export const api = {
  info: () => json<Info>('/v1/info'),
  usage: (sessionId?: string) =>
    json<UsageSummary>(`/v1/usage${sessionId ? `?session_id=${encodeURIComponent(sessionId)}` : ''}`),
  usageDaily: (days: number) => json<UsageAnalysis>(`/v1/usage/daily?days=${days}`),
  listSessions: () => json<{ sessions: SessionListRow[] }>('/v1/sessions'),
  createSession: (resume?: string, workspace?: string) =>
    json<Session>('/v1/sessions', {
      method: 'POST',
      body: JSON.stringify({ ...(resume ? { resume } : {}), ...(workspace ? { workspace } : {}) }),
    }),
  listWorkspaces: () => json<WorkspaceList>('/v1/workspaces'),
  addWorkspace: (path: string) => json<WorkspaceEntry>('/v1/workspaces', { method: 'POST', body: JSON.stringify({ path }) }),
  renameSession: (id: string, name: string) =>
    json<Session>(`/v1/sessions/${id}`, { method: 'PATCH', body: JSON.stringify({ name }) }),
  getSession: (id: string) => json<Session>(`/v1/sessions/${id}`),
  // history returns the raw JSON body: fromHistory parses it in wasm so tool
  // argument objects keep the key order the provider sent.
  history: (id: string) => text(`/v1/sessions/${id}/history`),
  getTurn: (id: string, turnId: string) => json<TurnSummary>(`/v1/sessions/${id}/turns/${turnId}`),
  cancelTurn: (id: string, turnId: string) => request(`/v1/sessions/${id}/turns/${turnId}/cancel`, { method: 'POST' }),
  listTasks: (id: string) => json<{ tasks: Task[] }>(`/v1/sessions/${id}/tasks`),
  context: (id: string) => json<ContextReport>(`/v1/sessions/${id}/context`),
  listMcp: (id: string) => json<{ servers: McpServer[] }>(`/v1/sessions/${id}/mcp`),
  getTask: (id: string, taskId: string) => json<TaskDetail>(`/v1/sessions/${id}/tasks/${taskId}`),
  cancelTask: (id: string, taskId: string) => request(`/v1/sessions/${id}/tasks/${taskId}/cancel`, { method: 'POST' }),
  reloadSandbox: () => json<Session['sandbox']>('/v1/sandbox/reload', { method: 'POST' }),
  approveBash: (id: string, approvalId: string) =>
    json<{ prompt: string }>(`/v1/sessions/${id}/approvals/${approvalId}`, { method: 'POST' }),
  compact: (id: string, focus: string, signal?: AbortSignal) =>
    json<Compaction>(`/v1/sessions/${id}/compact`, { method: 'POST', body: JSON.stringify({ focus }), signal }),
  listWorkflows: () => json<{ runs: WorkflowRun[] }>('/v1/workflows'),
  startWorkflow: (name: string, input: string) =>
    json<WorkflowView>('/v1/workflows', { method: 'POST', body: JSON.stringify({ name, input }) }),
  getWorkflow: (id: string) => json<WorkflowView>(`/v1/workflows/${id}`),
  resumeWorkflow: (id: string, retry?: string) =>
    json<WorkflowView>(`/v1/workflows/${id}/resume`, {
      method: 'POST',
      body: JSON.stringify(retry ? { retry } : {}),
    }),
  forkWorkflow: (id: string, afterStep: string) =>
    json<WorkflowView>(`/v1/workflows/${id}/fork`, { method: 'POST', body: JSON.stringify({ after_step: afterStep }) }),
  cancelWorkflow: (id: string) => json<WorkflowView>(`/v1/workflows/${id}/cancel`, { method: 'POST' }),
  approveWorkflow: (id: string) => json<WorkflowView>(`/v1/workflows/requests/${id}/approve`, { method: 'POST' }),
  rejectWorkflow: (id: string) => json<WorkflowView>(`/v1/workflows/requests/${id}/reject`, { method: 'POST' }),

  listAgentTasks: (params: { status?: string; workspace?: string; limit?: number; before?: string } = {}) => {
    const q = new URLSearchParams()
    if (params.status) q.set('status', params.status)
    if (params.workspace) q.set('workspace', params.workspace)
    if (params.limit !== undefined) q.set('limit', String(params.limit))
    if (params.before) q.set('before', params.before)
    const query = q.toString()
    return json<AgentTaskList>(`/v1/tasks${query ? `?${query}` : ''}`)
  },
  getAgentTask: (parentSession: string, taskId: string) =>
    json<AgentTaskDetail>(`/v1/tasks/${encodeURIComponent(parentSession)}/${encodeURIComponent(taskId)}`),

  // startTurn opens the turn's event stream from sequence 0.
  startTurn: (id: string, text: string, image?: { data: string; mime_type: string }) =>
    request(`/v1/sessions/${id}/turns`, { method: 'POST', body: JSON.stringify({ text, image, stream: true }) }),
  // attach re-reads a turn's events after sequence `after` (all of them when
  // omitted); used after a page reload or a dropped stream.
  attach: (id: string, turnId: string, after?: number) =>
    request(`/v1/sessions/${id}/turns/${turnId}/events${after === undefined ? '' : `?after=${after}`}`),
}

export interface TurnEvent {
  seq: number
  event: WireEvent
  /** The frame's `data` field, passed to reduce() unparsed. */
  raw: string
}

// events decodes an event-stream response into wire events with their
// sequence numbers.
export async function* events(res: Response): AsyncGenerator<TurnEvent> {
  if (!res.body) return
  for await (const frame of readSSE(res.body) as AsyncGenerator<Frame>) {
    yield { seq: frame.id ?? -1, event: JSON.parse(frame.data) as WireEvent, raw: frame.data }
  }
}
