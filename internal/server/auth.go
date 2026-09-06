package server

import (
	"crypto/subtle"
	"net/http"
	"strings"
)

// requireToken gates next behind "Authorization: Bearer <token>".
//
// Any web page a browser has open can send requests to a loopback port, but
// the browser never attaches our Authorization header on behalf of another
// origin. Only the page that received the token from the otto serve startup
// URL can therefore reach /v1/, which covers both CSRF and DNS rebinding
// without CORS or Origin checks. The token is accepted from the header only:
// a query parameter would end up in proxy access logs and Referer headers.
func requireToken(token string, next http.HandlerFunc) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		got, ok := strings.CutPrefix(r.Header.Get("Authorization"), "Bearer ")
		if !ok || subtle.ConstantTimeCompare([]byte(got), []byte(token)) != 1 {
			w.Header().Set("WWW-Authenticate", "Bearer")
			writeError(w, http.StatusUnauthorized, "unauthorized", "missing or invalid token")
			return
		}
		next(w, r)
	}
}
