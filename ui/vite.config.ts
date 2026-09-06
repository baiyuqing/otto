import { defineConfig } from 'vitest/config'
import react from '@vitejs/plugin-react'

// The build lands inside the Go module so `go build` embeds it
// (internal/server/ui.go). emptyOutDir stays false to keep dist/.gitkeep;
// `make ui` removes the previous build first.
// `npm run dev` proxies /v1 to a running `otto serve --listen`, so the page
// is same-origin with the API and needs no CORS.
export default defineConfig({
  plugins: [react()],
  build: { outDir: '../internal/server/ui/dist', emptyOutDir: false },
  server: { proxy: { '/v1': process.env.OTTO_URL ?? 'http://127.0.0.1:8787' } },
  test: { environment: 'node', include: ['src/**/*.test.ts'] },
})
