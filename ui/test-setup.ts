// vitest runs in Node, where the wasm package's default init() would fetch a
// file: URL. Hand it the bytes instead. The path is resolved from the vitest
// root (ui/) because import.meta.url is not a file URL under jsdom.
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import init from 'kite-web'

await init({ module_or_path: readFileSync(resolve(process.cwd(), '../crates/kite-web/pkg/kite_web_bg.wasm')) })
