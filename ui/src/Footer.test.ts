// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/react'
import { createElement } from 'react'
import { afterEach, describe, expect, it } from 'vitest'
import { Footer } from './Footer'

describe('Footer usage history', () => {
  afterEach(cleanup)

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
})
