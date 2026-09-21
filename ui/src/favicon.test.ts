import { existsSync, readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { describe, expect, it } from 'vitest'

const root = join(dirname(fileURLToPath(import.meta.url)), '..')
const html = readFileSync(join(root, 'index.html'), 'utf8')

describe('web UI page icon', () => {
  it('points the tab icon at a bundled file instead of the browser default', () => {
    const icon = html.match(/<link[^>]*rel="icon"[^>]*href="([^"]+)"/)
    expect(icon).not.toBeNull()
    // Vite only rewrites a relative href into the hashed assets/ output the
    // server routes; a root-relative one would 404 behind /assets/{*path}.
    expect(icon?.[1]).toMatch(/^\.\//)
    expect(icon?.[1]).toMatch(/\.svg$/)
    expect(existsSync(join(root, icon![1]))).toBe(true)
  })
})
