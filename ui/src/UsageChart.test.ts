// @vitest-environment jsdom
import mermaid from 'mermaid'
import { describe, expect, it } from 'vitest'
import { usageChart } from './UsageView'

describe('usageChart', () => {
  it('parses with the locked Mermaid version for a long range', async () => {
    mermaid.initialize({ startOnLoad: false, securityLevel: 'strict' })
    const daily = Array.from({ length: 90 }, (_, index) => ({
      date: `2026-${String(Math.floor(index / 28) + 1).padStart(2, '0')}-${String((index % 28) + 1).padStart(2, '0')}`,
      requests: 1,
      reported_requests: 1,
      input_tokens: 100 + index,
      output_tokens: 20 + index,
      cached_input_tokens: 50 + index,
    }))

    await expect(mermaid.parse(usageChart(daily))).resolves.toMatchObject({ diagramType: 'xychart' })
  })
})
