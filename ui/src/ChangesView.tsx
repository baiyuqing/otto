import { useCallback, useEffect, useRef, useState } from 'react'
import { api, type DiffFile, type SessionStatus, type WorkspaceDiff } from './api'

const fileLabel = (file: DiffFile) => (file.status === 'renamed' && file.old_path ? `${file.old_path} → ${file.path}` : file.path)

const lineClass = (line: string) => {
  if (line.startsWith('+++') || line.startsWith('---')) return undefined
  if (line.startsWith('+')) return 'diff-add'
  if (line.startsWith('-')) return 'diff-del'
  if (line.startsWith('@@')) return 'diff-hunk'
  return undefined
}

function Patch({ patch }: { patch: string }) {
  const lines = patch.split('\n')
  return (
    <pre className="changes-patch">
      {lines.map((line, i) => (
        <span key={i} className={lineClass(line)}>
          {line}
          {i < lines.length - 1 ? '\n' : ''}
        </span>
      ))}
    </pre>
  )
}

function File({ file }: { file: DiffFile }) {
  return (
    <details className="changes-file">
      <summary>
        <span className="changes-file-status">{file.status}</span>
        <span>{fileLabel(file)}</span>
      </summary>
      {file.binary ? <p className="changes-note">Binary file</p> : <Patch patch={file.patch} />}
      {file.truncated && <p className="changes-note">Truncated</p>}
    </details>
  )
}

export function ChangesView({
  workspace,
  status,
  onError,
}: {
  workspace: string
  status?: Map<string, SessionStatus>
  onError: (error: unknown) => void
}) {
  const [diff, setDiff] = useState<WorkspaceDiff | null>(null)
  // Only the latest request's response is shown, so a slow response for a
  // previous directory or an earlier Refresh cannot replace a newer one.
  const latest = useRef(0)

  const load = useCallback(() => {
    const request = ++latest.current
    api
      .getWorkspaceDiff(workspace)
      .then((got) => {
        if (request === latest.current) setDiff(got)
      })
      .catch((error) => {
        if (request === latest.current) onError(error)
      })
  }, [workspace, onError])

  useEffect(() => {
    setDiff(null)
    void load()
  }, [load])

  // Refetch when a session in this workspace finishes a turn (running ->
  // anything else) between two consecutive status snapshots.
  const previousStatus = useRef(status)
  useEffect(() => {
    const previous = previousStatus.current
    previousStatus.current = status
    if (!status || previous === status) return
    for (const [id, s] of status) {
      if (s.workspace !== workspace) continue
      if (previous?.get(id)?.turn === 'running' && s.turn !== 'running') {
        void load()
        return
      }
    }
  }, [status, workspace, load])

  return (
    <section className="changes-view" aria-labelledby="changes-title">
      <header className="changes-heading">
        <div>
          <h1 id="changes-title">Changes</h1>
          <p className="changes-path">{workspace}</p>
        </div>
        <button type="button" onClick={() => void load()}>
          Refresh
        </button>
      </header>

      {!diff ? (
        <p className="changes-loading">Loading changes…</p>
      ) : !diff.repository ? (
        <p className="changes-empty">Not a git repository</p>
      ) : (
        <>
          <p className="changes-branch">{diff.branch ?? '(no commits yet)'}</p>
          {diff.truncated && <p className="changes-note">Response truncated; some files are missing or cut short.</p>}
          {diff.files.length === 0 ? (
            <p className="changes-empty">No changes</p>
          ) : (
            diff.files.map((file) => <File key={file.path} file={file} />)
          )}
        </>
      )}
    </section>
  )
}
