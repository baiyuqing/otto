// The bridge to the macOS desktop app (desktop/), which loads this UI from
// otto serve and injects window.__OTTO_DESKTOP__ with an initialization
// script. Its openFolder navigates to a path the app intercepts, and the app
// then runs File > Open Folder…: the native folder picker, otto trust,
// POST /v1/workspaces, and a reload of this page.

interface OttoDesktop {
  openFolder: () => void
}

// desktopOpenFolder returns the desktop app's Open Folder action, or null in
// a plain browser, which has no way to learn an absolute folder path.
export function desktopOpenFolder(): (() => void) | null {
  const desktop = (window as unknown as { __OTTO_DESKTOP__?: OttoDesktop }).__OTTO_DESKTOP__
  return desktop?.openFolder ?? null
}
