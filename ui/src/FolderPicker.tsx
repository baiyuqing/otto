import { useEffect, useState } from 'react'
import { api, type DirListing } from './api'
import { workspaceName } from './uiText'

// FolderPicker browses the server's filesystem through GET /v1/fs/dirs. A
// browser cannot report the absolute path of a folder the user picks, so
// this is the browser's folder picker; the desktop app uses its native one.
export function FolderPicker(props: { onChoose: (path: string) => void; onCancel: () => void }) {
  const [listing, setListing] = useState<DirListing | null>(null)
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(true)

  const open = (path?: string) => {
    setLoading(true)
    setError('')
    api
      .listDirs(path)
      .then(setListing)
      .catch((e) => setError(e instanceof Error ? e.message : String(e)))
      .finally(() => setLoading(false))
  }

  useEffect(() => open(), [])

  return (
    <div className="modal-backdrop" onMouseDown={props.onCancel}>
      <div
        className="folder-picker"
        role="dialog"
        aria-modal="true"
        aria-labelledby="folder-picker-title"
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key === 'Escape') props.onCancel()
        }}
      >
        <h2 id="folder-picker-title">Choose a folder</h2>
        <div className="folder-picker-location">
          <button
            type="button"
            className="secondary"
            disabled={loading || !listing?.parent}
            onClick={() => listing?.parent && open(listing.parent)}
          >
            Up
          </button>
          <span className="folder-picker-path" title={listing?.path}>
            {listing?.path ?? ''}
          </span>
        </div>
        {listing && listing.roots.length > 1 && (
          <div className="folder-picker-roots">
            {listing.roots.map((root) => (
              <button type="button" key={root} className="secondary" disabled={loading} onClick={() => open(root)}>
                {workspaceName(root)}
              </button>
            ))}
          </div>
        )}
        <div className="folder-picker-list" aria-busy={loading}>
          {listing?.dirs.map((dir) => (
            <button type="button" key={dir.path} disabled={loading} onClick={() => open(dir.path)}>
              {dir.name}
            </button>
          ))}
          {listing && listing.dirs.length === 0 && <p className="folder-picker-empty">No subfolders</p>}
        </div>
        {error && <span role="alert">{error}</span>}
        <div className="rename-actions">
          <button type="button" className="secondary" onClick={props.onCancel}>
            Cancel
          </button>
          <button
            type="button"
            className="primary"
            disabled={loading || !listing}
            onClick={() => listing && props.onChoose(listing.path)}
          >
            Choose this folder
          </button>
        </div>
      </div>
    </div>
  )
}
