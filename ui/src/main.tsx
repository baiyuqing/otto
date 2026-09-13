import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { App } from './App'
import { ready } from './wire'
import './app.css'

// The transcript reducer and the wire types are wasm; nothing renders until
// the module is instantiated.
await ready

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
