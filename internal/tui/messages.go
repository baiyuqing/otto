package tui

import (
	"sync"

	"github.com/baiyuqing/otto/internal/agent"
	otmodel "github.com/baiyuqing/otto/internal/model"
)

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
	event                 *agent.Event
	compactionResult      *agent.CompactionResult
	aggregateUsage        otmodel.Usage
	aggregateUsagePresent bool
	err                   error
	done                  bool
	usesRegularEventSlot  bool
}

type turnStream struct {
	channel           chan turnEnvelope
	regularEventSlots chan struct{}
	generation        uint64
	abandonSignal     chan struct{}
	abandonOnce       sync.Once
}

func (s *turnStream) abandon() {
	if s == nil {
		return
	}
	s.abandonOnce.Do(func() {
		close(s.abandonSignal)
	})
}

type turnMsg struct {
	channel    <-chan turnEnvelope
	stream     *turnStream
	generation uint64
	value      turnEnvelope
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
