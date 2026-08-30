package memory

import "context"

type TurnMemory interface {
	Recall(context.Context, RecallRequest) (RecallResult, error)
	Observe(context.Context, Observation) (ObserveResult, error)
}

type Reader interface {
	Get(context.Context, RecordRef) (Record, error)
	GetByKey(context.Context, RecordKey) (Record, error)
	GetTombstone(context.Context, RecordRef) (Tombstone, error)
	GetCandidate(context.Context, CandidateRef) (Candidate, error)
	Search(context.Context, SearchRequest) (SearchResult, error)
}

type Manager interface {
	Reader
	Remember(context.Context, RememberRequest) (Record, error)
	Forget(context.Context, ForgetRequest) (ForgetResult, error)
	Review(context.Context, ReviewRequest) (ReviewResult, error)
}

type Proposer interface {
	Propose(context.Context, ProposeRequest) (CandidateBatch, error)
}

type Binding interface {
	TurnMemory
	Close() error
}

type Service interface {
	Manager
	Proposer
	Bind(context.Context, BindOptions) (Binding, error)
	Close() error
}

type Retriever interface {
	Retrieve(context.Context, RetrievalRequest) (RetrievalResult, error)
}

type Extractor interface {
	Extract(context.Context, ExtractRequest) ([]Proposal, error)
}

type Policy interface {
	Decide(context.Context, PolicyRequest) (PolicyDecision, error)
}

type Maintenance interface {
	Backup(context.Context, BackupRequest) (BackupInfo, error)
	ListBackups(context.Context) ([]BackupInfo, error)
	VerifyBackup(context.Context, string) (BackupInfo, error)
	Restore(context.Context, RestoreRequest) error
	PurgeForgotten(context.Context, PurgeForgottenRequest) (PurgeForgottenResult, error)
}

type ContentGuard interface {
	Check(context.Context, GuardInput) error
}

type Factory interface {
	Open(context.Context) (Components, error)
}

type Store interface {
	Identity(context.Context) (StoreIdentity, error)
	Get(context.Context, RecordRef) (Record, error)
	GetByKey(context.Context, RecordKey) (Record, error)
	GetTombstone(context.Context, RecordRef) (Tombstone, error)
	GetCandidate(context.Context, CandidateRef) (Candidate, error)
	List(context.Context, ListRequest) (RecordPage, error)
	ListTombstones(context.Context, TombstoneListRequest) (TombstonePage, error)
	ListCandidates(context.Context, CandidateListRequest) (CandidatePage, error)
	Upsert(context.Context, UpsertRequest) (Record, error)
	Forget(context.Context, StoreForgetRequest) (Tombstone, error)
	Propose(context.Context, ProposalBatch) (CandidateBatch, error)
	GetObservationReceipt(context.Context, string) (ObservationReceipt, error)
	CommitObservation(context.Context, ObservationCommit) (ObservationReceipt, error)
	Review(context.Context, StoreReviewRequest) (ReviewResult, error)
	Close() error
}
