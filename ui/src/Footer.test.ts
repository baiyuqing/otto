// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/react'
import { createElement } from 'react'
import { afterEach, describe, expect, it } from 'vitest'
import { Footer } from './Footer'

describe('Footer usage history', () => {
  afterEach(cleanup)

  it('shows process reasoning effort when present', () => {
    render(
      createElement(Footer, {
        info: {
          workspace: '/tmp/otto',
          provider: 'openai-compatible',
          profile: 'default',
          model: 'test',
          thinking: 'high',
          sandbox: 'seatbelt',
          profiles: ['default'],
        },
        session: null,
        turnUsage: null,
        recordedUsage: null,
      }),
    )

    expect(screen.getByText('openai-compatible · test · thinking high · seatbelt')).toBeTruthy()
  })

  it('shows persisted token totals and the weighted cache hit rate', () => {
    render(
      createElement(Footer, {
        info: null,
        session: null,
        turnUsage: null,
        recordedUsage: {
          requests: 3,
          reported_requests: 3,
          input_tokens: 200,
          output_tokens: 35,
          cached_input_tokens: 100,
          cache_hit_rate: 0.5,
        },
      }),
    )

    expect(screen.getByText('recorded in 200 out 35 cached 100 (50% hit)')).toBeTruthy()
  })

  it('shows the running turn status', () => {
    render(createElement(Footer, { info: null, session: null, turnUsage: null, recordedUsage: null, status: 'reasoning · 3s · turn 12s' }))

    expect(screen.getByText('reasoning · 3s · turn 12s')).toBeTruthy()
  })
})
