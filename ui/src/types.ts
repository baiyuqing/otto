// Wire types for otto serve, mirroring internal/server/openapi.yaml.

export interface Usage {
  input_tokens: number
  output_tokens: number
  cached_input_tokens?: number // omitted by the server when zero
}

export type TurnStatus = 'running' | 'ok' | 'error' | 'canceled'

export interface SessionTurn {
  id: string
  trigger: 'user' | 'task'
  status: TurnStatus
}

export interface Session {
  id: string
  name?: string
  workspace: string
  provider: string
  profile: string
  model: string
  context_window: number
  usage: Usage
  context_input_tokens: number
  sandbox: { mode: string; network: string; bash_available: boolean; summary: string }
  turn: SessionTurn | null
}

export interface SessionListRow {
  id: string
  name?: string
  path?: string
  workspace?: string
  provider?: string
  model?: string
  open: boolean
}

export interface TurnSummary {
  id: string
  trigger: 'user' | 'task'
  status: TurnStatus
  error?: string
  text: string
  usage: Usage
  usage_present: boolean
  started_at: string
  finished_at?: string
}

export interface Block {
  type: 'text' | 'tool_call' | 'tool_result'
  text?: string
  tool_call_id?: string
  tool_name?: string
  arguments?: unknown
  is_error?: boolean
}

export interface Message {
  id: string
  role: 'user' | 'assistant' | 'tool' | 'context'
  blocks: Block[]
  created_at: string
  display?: boolean
  context_type?: string
}

export interface Compaction {
  checkpoint_id?: string
  reason: string
  tokens_before: number
  estimated_tokens_after: number
  automatic: boolean
  usage?: Usage
  noop: boolean
}

export interface WireEvent {
  type:
    | 'agent_started'
    | 'agent_finished'
    | 'text_delta'
    | 'tool_call_started'
    | 'tool_call_finished'
    | 'provider_usage'
    | 'compaction_started'
    | 'compaction_planned'
    | 'compaction_completed'
    | 'compaction_warning'
    | 'memory_warning'
    | 'agent_error'
    | 'notification'
  turn_id?: string
  task_id?: string
  text?: string
  tool_name?: string
  tool_call_id?: string
  tool_args?: unknown
  result?: { content: string; is_error: boolean }
  usage?: Usage
  usage_present?: boolean
  compaction?: Compaction
  error?: string
}

export interface Task {
  id: string
  name?: string
  agent: string
  description: string
  model?: string
  status: 'queued' | 'running' | 'succeeded' | 'failed' | 'canceled'
  created_at: string
  started_at?: string
  finished_at?: string
  steps: number
  tool_calls: number
  last_tool?: string
  last_text?: string
  usage: Usage
  usage_present: boolean
  result?: string
  error?: string
}

export interface Info {
  workspace: string
  provider: string
  profile: string
  model: string
  sandbox: string
  profiles: string[]
}
