import { describe, expect, it } from 'vitest'
import { idleFollow, type FollowAction } from './follow'
import type { Session, SessionTurn } from './wire'

const sandbox = { mode: 'seatbelt', network: 'off', bash_available: true, summary: 'seatbelt' }

const session = (over: Partial<Session> = {}): Session => ({
  id: 's1',
  workspace: '/tmp/otto',
  provider: 'openai-compatible',
  profile: 'default',
  model: 'test',
  thinking: 'high',
  context_window: 128000,
  usage: { input_tokens: 0, output_tokens: 0 },
  context_input_tokens: 0,
  sandbox,
  turn: null,
  ...over,
})

const turn = (over: Partial<SessionTurn> = {}): SessionTurn => ({
  id: 'turn1',
  trigger: 'task',
  status: 'running',
  ...over,
})

describe('idleFollow', () => {
  it('does nothing when the session snapshot is unchanged', () => {
    const previous = session()
    expect(idleFollow(previous, session())).toEqual<FollowAction>({ kind: 'none' })
  })

  it('attaches when an idle page sees a running server-started turn', () => {
    expect(idleFollow(session(), session({ turn: turn() }))).toEqual<FollowAction>({
      kind: 'attach',
      turnId: 'turn1',
    })
  })

  it('does not re-attach to the running turn it is already following', () => {
    const previous = session({ turn: turn() })
    expect(idleFollow(previous, session({ turn: turn(), usage: { input_tokens: 3, output_tokens: 1 } }))).toEqual<FollowAction>({
      kind: 'none',
    })
  })

  it('reloads history when a wake finished between polls', () => {
    expect(
      idleFollow(
        session(),
        session({
          turn: turn({ status: 'ok' }),
          context_input_tokens: 40,
          usage: { input_tokens: 10, output_tokens: 8 },
        }),
      ),
    ).toEqual<FollowAction>({ kind: 'reload' })
  })

  it('reloads history when context grew without a live turn', () => {
    expect(idleFollow(session(), session({ context_input_tokens: 12 }))).toEqual<FollowAction>({ kind: 'reload' })
  })
})
