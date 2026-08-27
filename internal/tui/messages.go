package tui

import "github.com/baiyuqing/otto/internal/agent"

type overlayKind uint8

const (
	overlayNone overlayKind = iota
	overlayHelp
	overlaySession
)

type showHelpOverlayMsg struct{}

type showSessionOverlayMsg struct{}

type hideOverlayMsg struct{}

type toggleToolsMsg struct{}

type scrollViewportMsg struct {
	Delta int
}

type turnEnvelope struct {
	event *agent.Event
	err   error
	done  bool
}

type turnStream struct {
	channel    chan turnEnvelope
	eventSlots chan struct{}
}

type turnMsg struct {
	channel <-chan turnEnvelope
	stream  *turnStream
	value   turnEnvelope
}

type renderStreamingMsg struct {
	generation uint64
}

type newSessionResultMsg struct {
	generation uint64
	err        error
}

type ctrlCArmExpiredMsg struct {
	generation uint64
}
