package server

import (
	"embed"
	"io/fs"
	"net/http"
)

// uiDist holds the built web UI (`make ui` writes ui/dist). The directory
// is tracked with only a .gitkeep so a plain checkout still builds; the
// "all:" prefix embeds a directory that contains nothing but dotfiles.
//
//go:embed all:ui/dist
var uiDistFS embed.FS

var uiDist = func() fs.FS {
	sub, err := fs.Sub(uiDistFS, "ui/dist")
	if err != nil {
		panic(err) // embedded at build time; a missing dir is a build bug
	}
	return sub
}()

const uiPlaceholder = "Web UI not built; run make ui\n"

// uiHandlers serves dist's index.html at the root and everything under
// /assets/ as static files. Without an index.html the root returns a one-line
// placeholder so `otto serve --listen` on an unbuilt checkout still answers.
func uiHandlers(dist fs.FS) (index, assets http.Handler) {
	index = http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		page, err := fs.ReadFile(dist, "index.html")
		w.Header().Set("Cache-Control", "no-cache")
		if err != nil {
			w.Header().Set("Content-Type", "text/plain; charset=utf-8")
			w.WriteHeader(http.StatusOK)
			_, _ = w.Write([]byte(uiPlaceholder))
			return
		}
		w.Header().Set("Content-Type", "text/html; charset=utf-8")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write(page)
	})
	return index, http.FileServerFS(dist)
}
