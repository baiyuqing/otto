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
	m.registerActiveOperation(stream, cancel)
	m.turnHistoryBaseline = captureTurnHistoryBaseline(historyFromBackend(m.backend))
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

func (m *Model) applyCompactionEvent(event agent.Event, aggregateUsage otmodel.Usage, aggregateUsagePresent bool) bool {
	switch event.Type {
	case agent.EventCompactionStarted:
		m.statusText = "compacting context"
	case agent.EventCompactionCompleted:
		if event.Compaction == nil {
			return false
		}
		changed := m.reconcilePersistedToolResults()
		m.compactionCompleted = true
		return m.applyCompactionResult(compactionResultFromEvent(*event.Compaction), aggregateUsage, aggregateUsagePresent) || changed
	case agent.EventCompactionWarning:
		if event.Err != nil {
			m.statusText = event.Err.Error()
		} else {
			m.statusText = "context compaction warning"
		}
	}
	return false
}

func (m *Model) applyCompactionResult(result agent.CompactionResult, aggregateUsage otmodel.Usage, aggregateUsagePresent bool) bool {
	m.statusText = compactionStatus(result)
	if aggregateUsagePresent {
		m.usage = addUsageTotals(otmodel.Usage{}, &aggregateUsage)
	}
	return m.reconcilePersistedCheckpoint(result.CheckpointID, result.EstimatedTokensAfter)
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
	return compactionSummaryLabel(result.TokensBefore, result.EstimatedTokensAfter)
}

func compactionSummaryLabel(tokensBefore, tokensAfter int) string {
	before := formatCompactionTokenCount(tokensBefore)
	if tokensAfter > 0 {
		return fmt.Sprintf("[context] compacted %s → %s tokens", before, formatCompactionTokenCount(tokensAfter))
	}
	return fmt.Sprintf("[context] compacted %s tokens", before)
}

func formatCompactionTokenCount(tokens int) string {
	tokens = max(0, tokens)
	if tokens < 1000 {
		return fmt.Sprintf("%d", tokens)
	}
	whole := tokens / 1000
	frac := (tokens % 1000) / 100
	if whole >= 100 || frac == 0 {
		return fmt.Sprintf("%dk", whole)
	}
	return fmt.Sprintf("%d.%dk", whole, frac)
}
