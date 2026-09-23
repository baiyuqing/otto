import { fileURLToPath } from 'node:url'
import { defineConfig } from 'vitest/config'
import react from '@vitejs/plugin-react'

// The build lands in ui/dist, which crates/kite/src/server/ui.rs embeds into
// the binary at build time. emptyOutDir stays false to keep dist/.gitkeep;
// `make ui` removes the previous build first.
// `npm run dev` proxies /v1 to a running `kite serve --listen`, so the page
// is same-origin with the API and needs no CORS.
// `kite-web` is the wasm package `make ui` builds with wasm-pack; it lives
// outside ui/ and is gitignored, so it is aliased rather than installed.
export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      'kite-web': fileURLToPath(new URL('../crates/kite-web/pkg/kite_web.js', import.meta.url)),
    },
  },
  build: { outDir: 'dist', emptyOutDir: false },
  server: {
    proxy: { '/v1': process.env.KITE_URL ?? 'http://127.0.0.1:8787' },
    fs: { allow: ['..'] },
  },
  test: { environment: 'node', include: ['src/**/*.test.ts'], setupFiles: ['./test-setup.ts'] },
})
