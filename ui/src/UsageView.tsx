import { useEffect, useMemo, useState } from 'react'
import { api, type DailyUsage, type UsageAnalysis } from './api'
import { MermaidDiagram } from './TranscriptView'

const ranges = [7, 30, 90]
const integer = new Intl.NumberFormat()
const percent = new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 })

const values = (points: DailyUsage[], key: 'input_tokens' | 'output_tokens' | 'cached_input_tokens') =>
  points.map((point) => point[key]).join(', ')

export function usageChart(points: DailyUsage[]): string {
  const ceiling = Math.max(1, ...points.flatMap((point) => [point.input_tokens, point.output_tokens, point.cached_input_tokens]))
  return `---
config:
  themeVariables:
    xyChart:
      plotColorPalette: "#9a3412, #315f7d, #6b7f3b"
---
xychart
  x-axis "UTC day" 1 --> ${Math.max(1, points.length)}
  y-axis "Tokens" 0 --> ${ceiling}
  line [${values(points, 'input_tokens')}]
  line [${values(points, 'output_tokens')}]
  line [${values(points, 'cached_input_tokens')}]
`
}

export function UsageView({ onError }: { onError: (error: unknown) => void }) {
  const [days, setDays] = useState(30)
  const [analysis, setAnalysis] = useState<UsageAnalysis | null>(null)

  useEffect(() => {
    let canceled = false
    api
      .usageDaily(days)
      .then((next) => {
        if (!canceled) setAnalysis(next)
      })
      .catch((error) => {
        if (!canceled) onError(error)
      })
    return () => {
      canceled = true
    }
  }, [days, onError])

  const chart = useMemo(() => (analysis ? usageChart(analysis.daily) : ''), [analysis])

  return (
    <section className="usage-view" aria-labelledby="usage-title">
      <header className="usage-heading">
        <div>
          <h1 id="usage-title">Usage</h1>
          <p>Provider-reported token volume, grouped by UTC day.</p>
        </div>
        <div className="usage-ranges" role="group" aria-label="Usage range">
          {ranges.map((range) => (
            <button key={range} type="button" aria-pressed={days === range} onClick={() => setDays(range)}>
              {range} days
            </button>
          ))}
        </div>
      </header>

      {!analysis ? (
        <p className="usage-loading">Loading usage…</p>
      ) : (
        <>
          <div className="usage-metrics" role="group" aria-label="Usage totals">
            <Metric label="Input" value={integer.format(analysis.summary.input_tokens)} />
            <Metric label="Output" value={integer.format(analysis.summary.output_tokens)} />
            <Metric label="Cache hit" value={`${percent.format(analysis.summary.cache_hit_rate * 100)}%`} />
            <Metric label="Requests" value={integer.format(analysis.summary.requests)} />
          </div>

          {analysis.summary.requests === 0 ? (
            <div className="usage-empty">
              <h2>No usage in this range</h2>
              <p>Provider calls will appear here after their usage is reported.</p>
            </div>
          ) : (
            <>
              <div className="usage-chart-section">
                <div className="usage-section-heading">
                  <div>
                    <h2>Daily token volume</h2>
                    <span className="usage-date-range">
                      {analysis.daily[0]?.date} — {analysis.daily.at(-1)?.date} UTC
                    </span>
                  </div>
                  <div className="usage-legend" role="group" aria-label="Chart legend">
                    <span className="input">Input</span>
                    <span className="output">Output</span>
                    <span className="cached">Cached</span>
                  </div>
                </div>
                <MermaidDiagram code={chart} label="Daily input, output, and cached token chart" />
              </div>

              <div className="usage-table-section">
                <h2>Daily detail</h2>
                <table>
                  <thead>
                    <tr>
                      <th>Date (UTC)</th>
                      <th>Input</th>
                      <th>Output</th>
                      <th>Cached</th>
                      <th>Requests</th>
                    </tr>
                  </thead>
                  <tbody>
                    {analysis.daily.filter((point) => point.requests > 0).map((point) => (
                      <tr key={point.date}>
                        <td>{point.date}</td>
                        <td>{integer.format(point.input_tokens)}</td>
                        <td>{integer.format(point.output_tokens)}</td>
                        <td>{integer.format(point.cached_input_tokens)}</td>
                        <td>{integer.format(point.requests)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </>
          )}
        </>
      )}
    </section>
  )
}

function Metric({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <span>{label}</span>
      <strong>{value}</strong>
    </div>
  )
}
