package tui

import (
	"context"
	"errors"
	"fmt"
	"sync"

	tea "charm.land/bubbletea/v2"
	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	otmodel "github.com/baiyuqing/otto/internal/model"
)

func (m Model) startCompaction(focus string) (tea.Model, tea.Cmd) {
	ctx, cancel := context.WithCancel(rootContext(m.rootCtx))
	stream := newTurnStream()

	m.running = true
	m.cancel = cancel
	m.clearCtrlCArm()
	m.turnErrorSeen = false
	m.turnEventErr = nil
	m.statusText = ""
	m.dirtyStreaming = false
	m.renderTickActive = false
	m.activeAssistant = -1
	m.turnGeneration++
	m.operationKind = operationCompact
	m.compactionCompleted = false
	stream.generation = m.turnGeneration
	m.activeTurnStream = stream
	m.activeTurnChannel = stream.channel
	m.turnHistoryBaseline = turnHistoryBaseline{}
	m.turnEntryStart = len(m.entries)
	m.clearEditor()

	return m, startCompactionCommand(ctx, m.backend, focus, stream)
}

func startCompactionCommand(ctx context.Context, backend app.Backend, focus string, stream *turnStream) tea.Cmd {
	return func() tea.Msg {
		go runCompactionWorker(ctx, backend, focus, stream)
		return waitTurn(stream)()
	}
}

func runCompactionWorker(ctx context.Context, backend app.Backend, focus string, stream *turnStream) {
	defer close(stream.channel)

	if backend == nil {
		sendDurableTurnEnvelope(stream, turnEnvelope{err: errors.New("backend is required"), done: true})
		return
	}

	var eventMu sync.Mutex
	acceptingEvents := true
	result, err := backend.Compact(ctx, focus, func(event agent.Event) {
		eventMu.Lock()
		defer eventMu.Unlock()
		if !acceptingEvents {
			return
		}
		eventCopy := cloneAgentEvent(event)
		envelope := turnEnvelope{event: &eventCopy}
		if isCommittedCompactionCompletion(eventCopy) {
			envelope.aggregateUsage, envelope.aggregateUsagePresent = aggregateUsageSnapshot(backend)
		}
		sendTurnEvent(ctx, stream, envelope)
	})
	eventMu.Lock()
	acceptingEvents = false
	eventMu.Unlock()
	resultCopy := result
	aggregateUsage, aggregateUsagePresent := aggregateUsageSnapshot(backend)
	sendDurableTurnEnvelope(stream, turnEnvelope{
		compactionResult:      &resultCopy,
		aggregateUsage:        aggregateUsage,
		aggregateUsagePresent: aggregateUsagePresent,
		err:                   err,
		done:                  true,
	})
}

func (m *Model) applyCompactionEvent(event agent.Event, aggregateUsage otmodel.Usage, aggregateUsagePresent bool) {
	switch event.Type {
	case agent.EventCompactionStarted:
		m.statusText = "compacting context"
	case agent.EventCompactionCompleted:
		if event.Compaction == nil {
			return
		}
		m.compactionCompleted = true
		m.applyCompactionResult(compactionResultFromEvent(*event.Compaction), aggregateUsage, aggregateUsagePresent)
	case agent.EventCompactionWarning:
		if event.Err != nil {
			m.statusText = event.Err.Error()
		} else {
			m.statusText = "context compaction warning"
		}
	}
}

func (m *Model) applyCompactionResult(result agent.CompactionResult, aggregateUsage otmodel.Usage, aggregateUsagePresent bool) {
	m.statusText = compactionStatus(result)
	if aggregateUsagePresent {
		m.usage = addUsageTotals(otmodel.Usage{}, &aggregateUsage)
	}
}

func compactionResultFromEvent(event agent.CompactionEvent) agent.CompactionResult {
	return agent.CompactionResult{
		CheckpointID:         event.CheckpointID,
		Reason:               event.Reason,
		TokensBefore:         event.TokensBefore,
		EstimatedTokensAfter: event.EstimatedTokensAfter,
		Automatic:            event.Automatic,
		Usage:                event.Usage,
		UsagePresent:         event.UsagePresent,
		Noop:                 event.Noop,
	}
}

func compactionStatus(result agent.CompactionResult) string {
	if result.Noop {
		return "[context] no-op"
	}
	before := formatCompactionTokenCount(result.TokensBefore)
	if result.EstimatedTokensAfter > 0 {
		return fmt.Sprintf("[context] compacted %s → %s tokens", before, formatCompactionTokenCount(result.EstimatedTokensAfter))
	}
	return fmt.Sprintf("[context] compacted %s tokens", before)
}

func formatCompactionTokenCount(tokens int) string {
	if tokens < 1000 {
		return fmt.Sprintf("%d", tokens)
	}
	return fmt.Sprintf("%dk", tokens/1000)
}
