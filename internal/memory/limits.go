package memory

const (
	MaxRecordTextBytes       = 8 * 1024
	MaxNamespaceBytes        = 32
	MaxKindBytes             = 32
	MaxIDBytes               = 64
	MaxScopeIDBytes          = 128
	MaxSessionIDBytes        = 128
	MaxMessageIDBytes        = 128
	MaxSemanticKeyBytes      = 256
	MaxLabels                = 32
	MaxLabelBytes            = 64
	MaxMetadataEntries       = 32
	MaxMetadataKeyBytes      = 64
	MaxMetadataValueBytes    = 512
	MaxMetadataBytes         = 4 * 1024
	MaxReasonBytes           = 2 * 1024
	MaxProvenanceMessageIDs  = 32
	MaxQueryBytes            = 8 * 1024
	MaxObservationBytes      = 64 * 1024
	MaxObservationMessageIDs = 32
	MaxToolFacts             = 32
	MaxToolNameBytes         = 64
	MaxToolFactTextBytes     = 2 * 1024
	MaxFTSTerms              = 64
	MaxFTSTermBytes          = 256
	MaxBaselineRecords       = 16
	MaxRetrievalCandidates   = 256
	MaxCandidateScan         = 500
	MaxRequestScopes         = 16
	MaxRequestKinds          = 16
	MaxRequestLabels         = 16
	MaxPageSize              = 100
	MaxRecallRecords         = 64
	MaxTokenBudget           = 8192
	MaxCandidateBatch        = 8
	MaxCandidateBatchBytes   = 256 * 1024
	MaxGuardFields           = 512
	MaxGuardBytes            = 64 * 1024
	MaxExactGuardSpans       = 8192
	MaxExactGuardValues      = 64
	MaxExactGuardValueBytes  = 8 * 1024
	MaxCursorBytes           = 4 * 1024
	MaxCommitUnknownIDs      = 16
	MaxDuplicateIDRetries    = 8
)
