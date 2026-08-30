package memory

import "time"

type Namespace = string

type Origin string
type CandidateAction string
type CandidateState string
type ReviewDecision string
type CommitOperation string
type RecordState string
type PolicyDecision string

const (
	NamespaceUser      Namespace = "user"
	NamespaceWorkspace Namespace = "workspace"
)

const (
	OriginHuman     Origin = "human"
	OriginModel     Origin = "model"
	OriginExtractor Origin = "extractor"
	OriginImport    Origin = "import"
	OriginMigration Origin = "migration"
)

const (
	CandidateCreate CandidateAction = "create"
	CandidateUpdate CandidateAction = "update"
	CandidateForget CandidateAction = "forget"
)

const (
	CandidatePending  CandidateState = "pending"
	CandidateAccepted CandidateState = "accepted"
	CandidateRejected CandidateState = "rejected"
)

const (
	ReviewAccept ReviewDecision = "accept"
	ReviewReject ReviewDecision = "reject"
)

const (
	CommitSchema  CommitOperation = "schema"
	CommitUpsert  CommitOperation = "upsert"
	CommitForget  CommitOperation = "forget"
	CommitPropose CommitOperation = "propose"
	CommitObserve CommitOperation = "observe"
	CommitReview  CommitOperation = "review"
)

const (
	RecordActive    RecordState = "active"
	RecordTombstone RecordState = "tombstone"
)

const (
	PolicyAccept  PolicyDecision = "accept"
	PolicyPending PolicyDecision = "pending"
	PolicyReject  PolicyDecision = "reject"
)

type Scope struct {
	Namespace string
	ID        string
}

type Provenance struct {
	Origin         Origin
	SessionID      string
	MessageIDs     []string
	ObservationID  string
	DecisionAt     *time.Time
	DecisionSource Origin
}

type Record struct {
	ID         string
	Scope      Scope
	Kind       string
	Key        string
	Text       string
	Labels     []string
	Metadata   map[string]string
	Source     Provenance
	Confidence float64
	Revision   uint64
	CreatedAt  time.Time
	UpdatedAt  time.Time
	ExpiresAt  *time.Time
}

type Tombstone struct {
	ID          string
	Scope       Scope
	Revision    uint64
	CreatedAt   time.Time
	UpdatedAt   time.Time
	ForgottenAt time.Time
}

type Candidate struct {
	ID             string
	Proposed       Record
	Action         CandidateAction
	TargetID       string
	BaseRevision   uint64
	Reason         string
	State          CandidateState
	CreatedAt      time.Time
	DecidedAt      *time.Time
	DecisionSource Origin
	ResultRecordID string
	ResultRevision uint64
}

type RecordRef struct {
	Scope Scope
	ID    string
}

type RecordKey struct {
	Scope Scope
	Kind  string
	Key   string
}

type CandidateRef struct {
	Scope Scope
	ID    string
}

type StoreIdentity struct {
	DatabaseID    string
	UserScope     Scope
	SchemaVersion int
	Generation    uint64
}

type RecordPage struct {
	Records    []Record
	NextCursor string
}

type TombstonePage struct {
	Tombstones []Tombstone
	NextCursor string
}

type CandidatePage struct {
	Candidates []Candidate
	NextCursor string
}

type CandidateBatch struct {
	Candidates []Candidate
}

type ListRequest struct {
	Scopes         []Scope
	Kinds          []string
	Labels         []string
	Limit          int
	Cursor         string
	Now            time.Time
	IncludeExpired bool
}

type TombstoneListRequest struct {
	Scopes []Scope
	Limit  int
	Cursor string
}

type CandidateListRequest struct {
	Scopes []Scope
	States []CandidateState
	Limit  int
	Cursor string
}

type UpsertRequest struct {
	Record           Record
	ExpectedRevision *uint64
}

type StoreForgetRequest struct {
	Ref              RecordRef
	ExpectedRevision uint64
	ForgottenAt      time.Time
}

type ProposalBatch struct {
	Candidates []Candidate
}

type ObservationCommit struct {
	ObservationID string
	Candidates    []Candidate
	CreatedAt     time.Time
}

type ObservationReceipt struct {
	ObservationID string
	CandidateIDs  []string
	Existing      bool
}

type StoreReviewRequest struct {
	Ref            CandidateRef
	ResultRecordID string
	Decision       ReviewDecision
	Edited         *Record
	TargetRevision *uint64
	DecisionSource Origin
	DecidedAt      time.Time
}

type ReviewResult struct {
	Candidate Candidate
	Record    *Record
	Tombstone *Tombstone
}

type RetrievalRequest struct {
	Query           string
	Scopes          []Scope
	Kinds           []string
	Labels          []string
	IncludeExpired  bool
	IncludeBaseline bool
	Limit           int
	TokenBudget     int
	Now             time.Time
	EstimateTokens  func(string) int
	Cursor          string
}

type RetrievalMatch struct {
	Record Record
	Rank   int
}

type RetrievalResult struct {
	Matches    []RetrievalMatch
	UsedTokens int
	NextCursor string
}

type RecallRequest struct {
	Query       string
	Kinds       []string
	Limit       int
	TokenBudget int
}

type RecallResult struct {
	Records    []Record
	UsedTokens int
}

type ToolFact struct {
	ToolName string
	Text     string
}

type Observation struct {
	ID            string
	UserText      string
	AssistantText string
	SessionID     string
	ToolFacts     []ToolFact
	MessageIDs    []string
}

type ObserveResult struct {
	CandidateIDs []string
	Existing     bool
}

type SearchRequest struct {
	Query             string
	Scopes            []Scope
	Kinds             []string
	Labels            []string
	IncludeExpired    bool
	IncludeCandidates bool
	CandidateStates   []CandidateState
	Limit             int
	TokenBudget       int
	Cursor            string
	Now               time.Time
}

type SearchResult struct {
	Records    []Record
	Candidates []Candidate
	NextCursor string
}

type RememberRequest struct {
	ID               string
	Scope            Scope
	Kind             string
	Key              string
	Text             string
	Labels           []string
	Metadata         map[string]string
	Confidence       float64
	ExpiresAt        *time.Time
	ExpectedRevision *uint64
	Source           Provenance
}

type ForgetRequest struct {
	Ref              RecordRef
	ExpectedRevision uint64
	PurgeBackups     bool
	ConfirmPurge     bool
}

type ForgetResult struct {
	Tombstone       Tombstone
	PurgedBackupIDs []string
}

type ProposeRequest struct {
	Action       CandidateAction
	Scope        Scope
	Kind         string
	Key          string
	Text         string
	Labels       []string
	Metadata     map[string]string
	Confidence   float64
	ExpiresAt    *time.Time
	TargetID     string
	BaseRevision uint64
	Reason       string
	Source       Provenance
}

type ReviewRequest struct {
	Ref            CandidateRef
	Decision       ReviewDecision
	Edited         *Record
	TargetRevision *uint64
}

type ExtractRequest struct {
	Observation Observation
	Existing    []Record
}

type Proposal struct {
	Action       CandidateAction
	Scope        Scope
	Kind         string
	Key          string
	Text         string
	Labels       []string
	Metadata     map[string]string
	Confidence   float64
	ExpiresAt    *time.Time
	TargetID     string
	BaseRevision uint64
	Reason       string
}

type PolicyRequest struct {
	Origin     Origin
	Action     CandidateAction
	Scope      Scope
	Kind       string
	Confidence float64
	Source     Provenance
	Valid      bool
	Sensitive  bool
}

type GuardField struct {
	Name   string
	Value  string
	Opaque bool
}

type GuardInput struct {
	Fields []GuardField
}

type BindOptions struct {
	Scopes            []Scope
	DefaultWriteScope Scope
	Extractor         Extractor
	Guard             ContentGuard
	EstimateTokens    func(string) int
	Now               func() time.Time
}

type Capabilities struct {
	LexicalSearch       bool
	SemanticSearch      bool
	OnlineBackup        bool
	EncryptionAtRest    bool
	ConcurrentProcesses bool
}

type Components struct {
	Store        Store
	Retriever    Retriever
	Maintenance  Maintenance
	Capabilities Capabilities
}

type PurgeForgottenRequest struct {
	Tombstone Tombstone
	Confirm   bool
}

type PurgeForgottenResult struct {
	PurgedBackupIDs []string
}

type BackupRequest struct {
	Class string
}

type BackupInfo struct {
	ID             string
	CreatedAt      time.Time
	SchemaVersion  int
	DatabaseID     string
	Class          string
	DatabaseSHA256 string
	LedgerSHA256   string
	Bytes          int64
}

type RestoreRequest struct {
	Backup         string
	AllowMigration bool
	Confirm        bool
}
