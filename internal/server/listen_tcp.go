package server

import (
	"fmt"
	"net"
)

// ListenTCP creates a TCP listener on addr (host:port). Only loopback hosts
// are accepted: the token in Options.Token is the only thing separating the
// API from other local users, and it is not a substitute for TLS on a shared
// network. Port 0 picks a free port; read the result from Listener.Addr.
func ListenTCP(addr string) (net.Listener, error) {
	host, port, err := net.SplitHostPort(addr)
	if err != nil {
		return nil, fmt.Errorf("listen address %q: %w", addr, err)
	}
	host, ok := loopbackHost(host)
	if !ok {
		return nil, fmt.Errorf("listen address %q is not a loopback address; use 127.0.0.1, ::1, or localhost", addr)
	}
	listener, err := net.Listen("tcp", net.JoinHostPort(host, port))
	if err != nil {
		return nil, fmt.Errorf("listen on %s: %w", addr, err)
	}
	return listener, nil
}

// loopbackHost maps the literal "localhost" to 127.0.0.1 (no DNS lookup, so
// an /etc/hosts entry cannot redirect the bind) and accepts any loopback IP
// literal. Relax this function, not its callers, when a non-loopback bind
// behind TLS is added.
func loopbackHost(host string) (string, bool) {
	if host == "localhost" {
		return "127.0.0.1", true
	}
	ip := net.ParseIP(host)
	if ip == nil || !ip.IsLoopback() {
		return "", false
	}
	return host, true
}
