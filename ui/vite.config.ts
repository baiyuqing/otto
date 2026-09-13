import { fileURLToPath } from 'node:url'
import { defineConfig } from 'vitest/config'
import react from '@vitejs/plugin-react'

// The build lands inside the Go module so `go build` embeds it
// (internal/server/ui.go). emptyOutDir stays false to keep dist/.gitkeep;
// `make ui` removes the previous build first.
// `npm run dev` proxies /v1 to a running `otto serve --listen`, so the page
// is same-origin with the API and needs no CORS.
// `otto-web` is the wasm package `make ui` builds with wasm-pack; it lives
// outside ui/ and is gitignored, so it is aliased rather than installed.
export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      'otto-web': fileURLToPath(new URL('../crates/otto-web/pkg/otto_web.js', import.meta.url)),
    },
  },
  build: { outDir: '../internal/server/ui/dist', emptyOutDir: false },
  server: {
    proxy: { '/v1': process.env.OTTO_URL ?? 'http://127.0.0.1:8787' },
    fs: { allow: ['..'] },
  },
  test: { environment: 'node', include: ['src/**/*.test.ts'], setupFiles: ['./test-setup.ts'] },
})
