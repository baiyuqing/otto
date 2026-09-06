package server

import (
	"net"
	"strings"
	"testing"
)

func TestListenTCPRejectsNonLoopback(t *testing.T) {
	for _, addr := range []string{"0.0.0.0:0", ":0", "192.168.1.1:0", "example.com:0", "127.0.0.1"} {
		l, err := ListenTCP(addr)
		if err == nil {
			l.Close()
			t.Errorf("ListenTCP(%q) succeeded, want error", addr)
			continue
		}
		if !strings.Contains(err.Error(), addr) {
			t.Errorf("ListenTCP(%q) error = %q, want it to name the address", addr, err)
		}
	}
}

func TestListenTCPPortZeroResolvesAddress(t *testing.T) {
	l, err := ListenTCP("127.0.0.1:0")
	if err != nil {
		t.Fatalf("ListenTCP: %v", err)
	}
	defer l.Close()
	addr := l.Addr().(*net.TCPAddr)
	if addr.Port == 0 || !addr.IP.IsLoopback() {
		t.Fatalf("Addr() = %v, want a loopback address with a real port", addr)
	}
}

func TestListenTCPLocalhostAlias(t *testing.T) {
	l, err := ListenTCP("localhost:0")
	if err != nil {
		t.Fatalf("ListenTCP: %v", err)
	}
	defer l.Close()
	if got := l.Addr().(*net.TCPAddr).IP.String(); got != "127.0.0.1" {
		t.Fatalf("localhost bound to %s, want 127.0.0.1", got)
	}
}

func TestListenTCPIPv6Loopback(t *testing.T) {
	l, err := ListenTCP("[::1]:0")
	if err != nil {
		t.Skipf("IPv6 loopback unavailable: %v", err)
	}
	l.Close()
}
