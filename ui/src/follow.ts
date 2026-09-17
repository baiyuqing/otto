import type { Session } from './wire'

export const IDLE_POLL_MS = 1000

export type FollowAction = { kind: 'none' } | { kind: 'attach'; turnId: string } | { kind: 'reload' }

export function idleFollow(previous: Session, next: Session): FollowAction {
  const running = next.turn?.status === 'running' ? next.turn : null
  if (running) {
    if (previous.turn?.id === running.id && previous.turn.status === 'running') {
      return { kind: 'none' }
    }
    return { kind: 'attach', turnId: running.id }
  }
  if (progressed(previous, next)) {
    return { kind: 'reload' }
  }
  return { kind: 'none' }
}

function progressed(previous: Session, next: Session): boolean {
  return (
    previous.context_input_tokens !== next.context_input_tokens ||
    previous.usage.input_tokens !== next.usage.input_tokens ||
    previous.usage.output_tokens !== next.usage.output_tokens ||
    (previous.usage.cached_input_tokens ?? 0) !== (next.usage.cached_input_tokens ?? 0) ||
    previous.turn?.id !== next.turn?.id ||
    previous.turn?.status !== next.turn?.status
  )
}
