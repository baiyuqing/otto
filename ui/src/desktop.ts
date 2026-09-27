// The bridge to the macOS desktop app (desktop/), which loads this UI from
// otto serve and injects window.__TAURI__ (app.withGlobalTauri). Its
// pick_directory command shows the native folder picker and resolves to the
// chosen absolute path, or null when cancelled.

interface TauriGlobal {
  core: { invoke: (command: string) => Promise<unknown> }
}

// nativeFolderPicker returns the desktop app's folder picker, or null in a
// plain browser, which has no way to learn an absolute folder path.
export function nativeFolderPicker(): (() => Promise<string | null>) | null {
  const tauri = (window as unknown as { __TAURI__?: TauriGlobal }).__TAURI__
  if (!tauri?.core?.invoke) return null
  return async () => {
    const picked = await tauri.core.invoke('pick_directory')
    return typeof picked === 'string' ? picked : null
  }
}
