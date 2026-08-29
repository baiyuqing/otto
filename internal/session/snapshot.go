package session

import "github.com/baiyuqing/otto/internal/model"

func (m *Memory) Snapshot() Snapshot {
	m.mu.Lock()
	defer m.mu.Unlock()
	return snapshotFromState(m.aggregateUsage, m.usagePresent, m.messages, m.latestCompaction, m.hasLatestCompaction)
}

func (s *Store) Snapshot() Snapshot {
	s.mu.Lock()
	defer s.mu.Unlock()
	return snapshotFromState(s.aggregateUsage, s.usagePresent, s.messages, s.latestCompaction, s.hasLatestCompaction)
}

func snapshotFromState(aggregateUsage model.Usage, aggregateUsagePresent bool, messages []model.Message, latestCompaction CompactionMetadata, hasLatestCompaction bool) Snapshot {
	snapshot := Snapshot{
		AggregateUsage:        aggregateUsage,
		AggregateUsagePresent: aggregateUsagePresent,
	}
	inputTokens, present, pending := latestContextInputTokens(messages, latestCompaction, hasLatestCompaction)
	snapshot.ContextInputTokens = inputTokens
	snapshot.ContextInputTokensPresent = present
	snapshot.ContextInputTokensPending = pending
	return snapshot
}

func latestContextInputTokens(messages []model.Message, latestCompaction CompactionMetadata, hasLatestCompaction bool) (int, bool, bool) {
	start := 0
	if hasLatestCompaction {
		if latestCompaction.FirstPostCheckpointMessageID == "" {
			return 0, false, true
		}
		start = -1
		for index := range messages {
			if messages[index].ID == latestCompaction.FirstPostCheckpointMessageID {
				start = index
				break
			}
		}
		if start < 0 {
			return 0, false, true
		}
	}
	for index := len(messages) - 1; index >= start; index-- {
		if messages[index].Role != model.RoleAssistant {
			continue
		}
		if hasMeaningfulInputUsage(messages[index].Usage) {
			return messages[index].Usage.InputTokens, true, false
		}
		return 0, false, false
	}
	if hasLatestCompaction {
		return 0, false, true
	}
	return 0, false, false
}

func hasMeaningfulInputUsage(usage *model.Usage) bool {
	return usage != nil && usage.InputTokens > 0
}
