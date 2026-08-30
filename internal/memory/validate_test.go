package memory

import (
	"errors"
	"math"
	"reflect"
	"strings"
	"testing"
	"time"
)

var testNow = time.Date(2026, 8, 29, 12, 0, 0, 0, time.UTC)

func validScope() Scope { return Scope{Namespace: NamespaceUser, ID: "u-1"} }

func validProvenance() Provenance {
	return Provenance{Origin: OriginHuman, SessionID: "s-1", MessageIDs: []string{"m-1"}, ObservationID: "o-1"}
}

func validRecord() Record {
	return Record{
		ID: "r-1", Scope: validScope(), Kind: "preference", Key: "editor.theme", Text: "Uses a dark theme",
		Labels: []string{"editor", "theme"}, Metadata: map[string]string{"source_type": "explicit"},
		Source: validProvenance(), Confidence: 0.9, Revision: 1, CreatedAt: testNow, UpdatedAt: testNow,
	}
}

func validCandidate() Candidate {
	r := validRecord()
	r.ID, r.Revision, r.CreatedAt, r.UpdatedAt = "", 0, time.Time{}, time.Time{}
	r.Source = Provenance{Origin: OriginModel, SessionID: "s-1", MessageIDs: []string{"m-1"}}
	return Candidate{ID: "c-1", Proposed: r, Action: CandidateCreate, State: CandidatePending, CreatedAt: testNow}
}

func assertSafeError(t *testing.T, err, target error, rejected ...string) {
	t.Helper()
	if !errors.Is(err, target) {
		t.Fatalf("error = %v, want %v", err, target)
	}
	for _, value := range rejected {
		if value != "" && strings.Contains(err.Error(), value) {
			t.Fatalf("error echoed rejected content: %q", err)
		}
	}
}

func TestValidateScopeBoundariesAndGrammar(t *testing.T) {
	validNamespace := "a" + strings.Repeat("x", MaxNamespaceBytes-1)
	validID := strings.Repeat("I", MaxScopeIDBytes)
	for _, scope := range []Scope{{Namespace: validNamespace, ID: validID}, validScope()} {
		if err := ValidateScope(scope); err != nil {
			t.Fatalf("exact boundary rejected: %v", err)
		}
	}
	cases := []Scope{
		{Namespace: "a" + strings.Repeat("x", MaxNamespaceBytes), ID: "id"},
		{Namespace: "UPPER", ID: "id"}, {Namespace: "1bad", ID: "id"}, {Namespace: "bad namespace", ID: "id"},
		{Namespace: "user", ID: strings.Repeat("I", MaxScopeIDBytes+1)}, {Namespace: "user", ID: "bad/id"},
		{Namespace: "user", ID: "bad\x00id"},
	}
	for _, scope := range cases {
		if err := ValidateScope(scope); !errors.Is(err, ErrInvalidRequest) {
			t.Errorf("ValidateScope(%q) = %v", scope.Namespace, err)
		}
	}
}

func TestValidateRecordEveryByteAndCountBoundary(t *testing.T) {
	textFields := []struct {
		name string
		max  int
		set  func(*Record, string)
	}{
		{"ID", MaxIDBytes, func(r *Record, v string) { r.ID = v }},
		{"text", MaxRecordTextBytes, func(r *Record, v string) { r.Text = v }},
		{"kind", MaxKindBytes, func(r *Record, v string) { r.Kind = v }},
		{"key", MaxSemanticKeyBytes, func(r *Record, v string) { r.Key = v }},
		{"label", MaxLabelBytes, func(r *Record, v string) { r.Labels = []string{v} }},
		{"metadata key", MaxMetadataKeyBytes, func(r *Record, v string) { r.Metadata = map[string]string{v: "v"} }},
		{"metadata value", MaxMetadataValueBytes, func(r *Record, v string) { r.Metadata = map[string]string{"k": v} }},
		{"session ID", MaxSessionIDBytes, func(r *Record, v string) { r.Source.SessionID = v }},
		{"message ID", MaxMessageIDBytes, func(r *Record, v string) { r.Source.MessageIDs = []string{v} }},
		{"observation ID", MaxIDBytes, func(r *Record, v string) { r.Source.ObservationID = v }},
	}
	for _, tc := range textFields {
		t.Run(tc.name, func(t *testing.T) {
			r := validRecord()
			exact := strings.Repeat("x", tc.max)
			tc.set(&r, exact)
			if err := ValidateRecord(r); err != nil {
				t.Fatalf("exact %d bytes: %v", tc.max, err)
			}
			r = validRecord()
			over := strings.Repeat("x", tc.max+1)
			tc.set(&r, over)
			assertSafeError(t, ValidateRecord(r), ErrInvalidRecord, over)
		})
	}

	r := validRecord()
	r.Labels = make([]string, MaxLabels)
	for i := range r.Labels {
		r.Labels[i] = "x" + strings.Repeat("a", i)
	}
	if err := ValidateRecord(r); err != nil {
		t.Fatalf("exact label count: %v", err)
	}
	r.Labels = append(r.Labels, "one-over")
	assertSafeError(t, ValidateRecord(r), ErrInvalidRecord, "one-over")

	r = validRecord()
	r.Metadata = make(map[string]string, MaxMetadataEntries)
	for i := 0; i < MaxMetadataEntries; i++ {
		r.Metadata[string(rune('a'+i%26))+strings.Repeat("x", i/26)] = ""
	}
	if err := ValidateRecord(r); err != nil {
		t.Fatalf("exact metadata count: %v", err)
	}
	r.Metadata["one-over"] = "LEAK_metadata_count"
	assertSafeError(t, ValidateRecord(r), ErrInvalidRecord, "LEAK_metadata_count")

	r = validRecord()
	r.Metadata = make(map[string]string, 8)
	for i := 0; i < 8; i++ {
		r.Metadata[string(rune('a'+i))] = strings.Repeat("x", 511)
	}
	if err := ValidateRecord(r); err != nil {
		t.Fatalf("exact metadata total: %v", err)
	}
	r.Metadata["a"] += "x"
	assertSafeError(t, ValidateRecord(r), ErrInvalidRecord, r.Metadata["a"])

	r = validRecord()
	r.Source.MessageIDs = make([]string, MaxProvenanceMessageIDs)
	for i := range r.Source.MessageIDs {
		r.Source.MessageIDs[i] = "m" + strings.Repeat("x", i)
	}
	if err := ValidateRecord(r); err != nil {
		t.Fatalf("exact provenance ID count: %v", err)
	}
	r.Source.MessageIDs = append(r.Source.MessageIDs, "over")
	assertSafeError(t, ValidateRecord(r), ErrInvalidRecord, "over")
}

func TestValidateRecordRejectsOneBytePastTextLimit(t *testing.T) {
	record := validRecord()
	record.Text = strings.Repeat("x", MaxRecordTextBytes+1)
	err := ValidateRecord(record)
	assertSafeError(t, err, ErrInvalidRecord, record.Text)
}

func TestValidateRecordGrammarEnumsNumbersAndTimes(t *testing.T) {
	invalidUTF8 := "LEAK_" + string([]byte{0xff})
	controls := []string{"LEAK_\x00", "LEAK_\x1f", "LEAK_\x7f", "LEAK_\u0085"}
	for _, value := range append([]string{invalidUTF8}, controls...) {
		r := validRecord()
		r.Text = value
		assertSafeError(t, ValidateRecord(r), ErrInvalidRecord, value)
	}
	for _, mutate := range []func(*Record){
		func(r *Record) { r.ID = "bad/id" }, func(r *Record) { r.Kind = "Bad" },
		func(r *Record) { r.Key = " key" }, func(r *Record) { r.Labels = []string{" label"} },
		func(r *Record) { r.Metadata = map[string]string{" key": "v"} },
		func(r *Record) { r.Labels = []string{"Alpha", "alpha"} },
		func(r *Record) { r.Metadata = map[string]string{"Alpha": "1", "alpha": "2"} },
		func(r *Record) { r.Source.Origin = Origin("unknown") },
		func(r *Record) { r.Source.DecisionSource = OriginModel; r.Source.DecisionAt = nil },
		func(r *Record) { x := testNow; r.Source.DecisionAt = &x; r.Source.DecisionSource = "" },
		func(r *Record) { r.Confidence = math.NaN() }, func(r *Record) { r.Confidence = math.Inf(1) },
		func(r *Record) { r.Confidence = -0.01 }, func(r *Record) { r.Confidence = 1.01 },
		func(r *Record) { r.Revision = 0 }, func(r *Record) { r.CreatedAt = time.Time{} },
		func(r *Record) { r.CreatedAt = testNow.In(time.FixedZone("offset", 0)) },
		func(r *Record) { r.CreatedAt = testNow.Add(time.Second); r.UpdatedAt = testNow },
		func(r *Record) { x := testNow.Add(-time.Second); r.ExpiresAt = &x },
		func(r *Record) {
			x := testNow.Add(time.Second)
			r.Source.DecisionAt = &x
			r.Source.DecisionSource = OriginHuman
		},
	} {
		r := validRecord()
		mutate(&r)
		if err := ValidateRecord(r); !errors.Is(err, ErrInvalidRecord) {
			t.Errorf("error = %v", err)
		}
	}
	for _, confidence := range []float64{0, 1} {
		r := validRecord()
		r.Confidence = confidence
		if err := ValidateRecord(r); err != nil {
			t.Errorf("confidence %v: %v", confidence, err)
		}
	}

	for _, origin := range []Origin{OriginModel, OriginExtractor, OriginImport} {
		r := validRecord()
		r.Source.Origin = origin
		if err := ValidateRecord(r); !errors.Is(err, ErrInvalidRecord) {
			t.Errorf("undecided active %s: %v", origin, err)
		}
		d := testNow
		r.Source.DecisionAt, r.Source.DecisionSource = &d, OriginMigration
		if err := ValidateRecord(r); err != nil {
			t.Errorf("decided active %s: %v", origin, err)
		}
	}
}

func TestValidateRecordTimestampYearBoundaries(t *testing.T) {
	for _, year := range []int{1, 9999} {
		r := validRecord()
		stamp := time.Date(year, 2, 1, 0, 0, 0, 0, time.UTC)
		r.CreatedAt, r.UpdatedAt = stamp, stamp
		if err := ValidateRecord(r); err != nil {
			t.Errorf("year %d: %v", year, err)
		}
	}
	for _, year := range []int{0, 10000} {
		r := validRecord()
		stamp := time.Date(year, 2, 1, 0, 0, 0, 0, time.UTC)
		r.CreatedAt, r.UpdatedAt = stamp, stamp
		if err := ValidateRecord(r); !errors.Is(err, ErrInvalidRecord) {
			t.Errorf("year %d = %v", year, err)
		}
	}
}

func TestValidateRecordDoesNotMutateInput(t *testing.T) {
	r := validRecord()
	r.Labels = []string{"Zulu", "alpha"}
	r.Metadata = map[string]string{"Zulu": "1", "alpha": "2"}
	before := CloneRecord(r)
	if err := ValidateRecord(r); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(r, before) {
		t.Fatalf("record mutated: %#v != %#v", r, before)
	}
}

func TestValidateCandidateRulesAndBoundaries(t *testing.T) {
	c := validCandidate()
	c.Reason = strings.Repeat("r", MaxReasonBytes)
	if err := ValidateCandidate(c); err != nil {
		t.Fatalf("exact reason: %v", err)
	}
	c.Reason += "x"
	assertSafeError(t, ValidateCandidate(c), ErrInvalidRecord, c.Reason)

	for _, action := range []CandidateAction{CandidateUpdate, CandidateForget} {
		c = validCandidate()
		c.Action = action
		c.TargetID = "r-1"
		c.BaseRevision = 1
		if action == CandidateForget {
			c.Proposed = Record{Scope: validScope(), Source: Provenance{Origin: OriginModel}}
		}
		if err := ValidateCandidate(c); err != nil {
			t.Errorf("valid %s: %v", action, err)
		}
	}
	cases := []func(*Candidate){
		func(c *Candidate) { c.Action = CandidateAction("unknown") },
		func(c *Candidate) { c.State = CandidateState("unknown") },
		func(c *Candidate) { c.Action = CandidateCreate; c.TargetID = "r-1" },
		func(c *Candidate) { c.Action = CandidateCreate; c.BaseRevision = 1 },
		func(c *Candidate) { c.Action = CandidateUpdate },
		func(c *Candidate) { c.Action = CandidateForget },
		func(c *Candidate) { c.Proposed.Source.Origin = OriginHuman },
		func(c *Candidate) { c.Proposed.ID = "r-1" }, func(c *Candidate) { c.Proposed.Revision = 1 },
		func(c *Candidate) { c.DecidedAt = ptrTime(testNow); c.DecisionSource = OriginHuman },
		func(c *Candidate) { c.DecisionSource = OriginHuman }, func(c *Candidate) { c.ResultRecordID = "r-1" },
	}
	for _, mutate := range cases {
		c = validCandidate()
		mutate(&c)
		if err := ValidateCandidate(c); !errors.Is(err, ErrInvalidRecord) {
			t.Errorf("candidate error = %v", err)
		}
	}
}

func TestValidateCandidateIDByteBoundary(t *testing.T) {
	candidate := validCandidate()
	candidate.ID = strings.Repeat("c", MaxIDBytes)
	if err := ValidateCandidate(candidate); err != nil {
		t.Fatalf("exact candidate ID: %v", err)
	}
	candidate.ID += "c"
	assertSafeError(t, ValidateCandidate(candidate), ErrInvalidRecord, candidate.ID)
}

func TestValidateCandidateDoesNotMutateInput(t *testing.T) {
	candidate := validCandidate()
	before := CloneCandidate(candidate)
	if err := ValidateCandidate(candidate); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(candidate, before) {
		t.Fatal("candidate validator mutated input")
	}
}

func TestValidateCandidateRequiresTargetRevisionForUpdate(t *testing.T) {
	candidate := validCandidate()
	candidate.Action = CandidateUpdate
	candidate.TargetID = ""
	candidate.BaseRevision = 0
	if err := ValidateCandidate(candidate); !errors.Is(err, ErrInvalidRecord) {
		t.Fatalf("error = %v, want ErrInvalidRecord", err)
	}
}

func TestValidateCandidateDecisionRelationships(t *testing.T) {
	accepted := validCandidate()
	accepted.State = CandidateAccepted
	accepted.DecidedAt = ptrTime(testNow)
	accepted.DecisionSource = OriginHuman
	accepted.ResultRecordID, accepted.ResultRevision = "new-r", 1
	accepted.Proposed = Record{Scope: validScope()}
	if err := ValidateCandidate(accepted); err != nil {
		t.Fatalf("accepted create: %v", err)
	}
	for _, mutate := range []func(*Candidate){
		func(c *Candidate) { c.DecidedAt = ptrTime(testNow.Add(-time.Second)) },
		func(c *Candidate) { c.DecisionSource = OriginModel }, func(c *Candidate) { c.Reason = "not retained" },
		func(c *Candidate) { c.Proposed.Text = "not retained" }, func(c *Candidate) { c.ResultRecordID = "" },
		func(c *Candidate) { c.ResultRevision = 0 },
	} {
		c := accepted
		mutate(&c)
		if err := ValidateCandidate(c); !errors.Is(err, ErrInvalidRecord) {
			t.Errorf("accepted invalid = %v", err)
		}
	}

	rejected := accepted
	rejected.State = CandidateRejected
	rejected.ResultRecordID = ""
	rejected.ResultRevision = 0
	if err := ValidateCandidate(rejected); err != nil {
		t.Fatalf("rejected: %v", err)
	}
}

func TestValidateRequestScopeFilterPageCursorAndBudgetRules(t *testing.T) {
	validList := ListRequest{Scopes: []Scope{validScope()}, Limit: MaxPageSize, Cursor: strings.Repeat("c", MaxCursorBytes), IncludeExpired: true}
	if err := ValidateListRequest(validList); err != nil {
		t.Fatalf("exact list: %v", err)
	}
	validRetrieval := RetrievalRequest{Query: strings.Repeat("q", MaxQueryBytes), Scopes: []Scope{validScope()}, Limit: MaxPageSize, TokenBudget: MaxTokenBudget, Now: testNow}
	if err := ValidateRetrievalRequest(validRetrieval); err != nil {
		t.Fatalf("exact retrieval: %v", err)
	}
	validSearch := SearchRequest{Query: "q", Scopes: []Scope{validScope()}, Limit: MaxPageSize, TokenBudget: MaxTokenBudget, Now: testNow}
	if err := ValidateSearchRequest(validSearch); err != nil {
		t.Fatalf("search: %v", err)
	}

	for _, req := range []ListRequest{
		{Scopes: []Scope{validScope()}, Limit: 0}, {Scopes: []Scope{validScope()}, Limit: MaxPageSize + 1},
		{Scopes: []Scope{validScope()}, Limit: 1, Cursor: strings.Repeat("c", MaxCursorBytes+1)},
		{Scopes: []Scope{validScope()}, Limit: 1, Cursor: "bad\x00cursor"},
		{Scopes: []Scope{validScope()}, Kinds: []string{" kind"}, Limit: 1},
		{Scopes: []Scope{validScope()}, Labels: []string{" label"}, Limit: 1},
		{Scopes: []Scope{validScope()}, Limit: 1, IncludeExpired: false},
	} {
		if err := ValidateListRequest(req); !errors.Is(err, ErrInvalidRequest) {
			t.Errorf("list = %v", err)
		}
	}

	manyScopes := make([]Scope, MaxRequestScopes)
	for i := range manyScopes {
		manyScopes[i] = Scope{Namespace: "custom", ID: "i" + strings.Repeat("x", i)}
	}
	if err := ValidateListRequest(ListRequest{Scopes: manyScopes, Limit: 1, IncludeExpired: true}); err != nil {
		t.Fatalf("exact scope count: %v", err)
	}
	manyScopes = append(manyScopes, Scope{Namespace: "custom", ID: "over"})
	if err := ValidateListRequest(ListRequest{Scopes: manyScopes, Limit: 1, IncludeExpired: true}); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("scope count = %v", err)
	}
	if err := ValidateListRequest(ListRequest{Limit: 1, IncludeExpired: true}); err != nil {
		t.Errorf("defensive empty store scopes rejected: %v", err)
	}
	if err := ValidateSearchRequest(SearchRequest{Limit: 1, TokenBudget: 1, Now: testNow}); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("empty search scopes = %v", err)
	}

	for _, req := range []RetrievalRequest{
		{Query: strings.Repeat("q", MaxQueryBytes+1), Scopes: []Scope{validScope()}, Limit: 1, TokenBudget: 1, Now: testNow},
		{Query: "q", Scopes: []Scope{validScope()}, Limit: 0, TokenBudget: 1, Now: testNow},
		{Query: "q", Scopes: []Scope{validScope()}, Limit: 1, TokenBudget: 0, Now: testNow},
		{Query: "q", Scopes: []Scope{validScope()}, Limit: 1, TokenBudget: MaxTokenBudget + 1, Now: testNow},
		{Query: "q", Scopes: []Scope{{Namespace: "custom", ID: "x"}}, Limit: 1, TokenBudget: 1, Now: testNow},
		{Query: "q", Scopes: []Scope{validScope(), validScope()}, Limit: 1, TokenBudget: 1, Now: testNow},
		{Query: "q", Scopes: []Scope{{Namespace: NamespaceWorkspace, ID: "w"}}, Limit: 1, TokenBudget: 1, Now: testNow},
	} {
		if err := ValidateRetrievalRequest(req); !errors.Is(err, ErrInvalidRequest) {
			t.Errorf("retrieval = %v", err)
		}
	}

	if err := ValidateSearchRequest(SearchRequest{Query: "q", Scopes: []Scope{validScope()}, IncludeCandidates: false, CandidateStates: []CandidateState{CandidatePending}, Limit: 1, TokenBudget: 1, Now: testNow}); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("candidate state without inclusion = %v", err)
	}
	if err := ValidateSearchRequest(SearchRequest{Query: "q", Scopes: []Scope{validScope()}, IncludeCandidates: true, CandidateStates: []CandidateState{"unknown"}, Limit: 1, TokenBudget: 1, Now: testNow}); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("unknown candidate state = %v", err)
	}
	if err := ValidateListRequest(ListRequest{Scopes: []Scope{validScope()}, Labels: []string{"Alpha", "alpha"}, Limit: 1, IncludeExpired: true}); err != nil {
		t.Errorf("case-sensitive label filters rejected: %v", err)
	}
	if err := ValidateListRequest(ListRequest{Scopes: []Scope{validScope()}, Limit: 1, Cursor: "bad\x00cursor", IncludeExpired: true}); !errors.Is(err, ErrInvalidCursor) {
		t.Errorf("invalid cursor sentinel = %v", err)
	}
}

func TestValidateRequestExactCollectionBoundariesAndNoMutation(t *testing.T) {
	req := ListRequest{Scopes: []Scope{validScope()}, Kinds: make([]string, MaxRequestKinds), Labels: make([]string, MaxRequestLabels), Limit: 1, IncludeExpired: true}
	for i := range req.Kinds {
		req.Kinds[i] = "k" + strings.Repeat("x", i)
	}
	for i := range req.Labels {
		req.Labels[i] = "l" + strings.Repeat("x", i)
	}
	before := CloneListRequest(req)
	if err := ValidateListRequest(req); err != nil {
		t.Fatalf("exact filters: %v", err)
	}
	if !reflect.DeepEqual(req, before) {
		t.Fatal("validator mutated request")
	}
	req.Kinds = append(req.Kinds, "over")
	if err := ValidateListRequest(req); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("kind count = %v", err)
	}
	req = before
	req.Labels = append(req.Labels, "over")
	if err := ValidateListRequest(req); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("label count = %v", err)
	}
}

func TestValidateRecallAndExtractionBoundaries(t *testing.T) {
	recall := RecallRequest{Query: "q", Limit: MaxRecallRecords, TokenBudget: MaxTokenBudget}
	if err := ValidateRecallRequest(recall); err != nil {
		t.Fatalf("exact recall: %v", err)
	}
	recall.Limit++
	if err := ValidateRecallRequest(recall); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("recall one-over = %v", err)
	}

	extract := ExtractRequest{Observation: Observation{ID: "o-1"}, Existing: make([]Record, MaxBaselineRecords)}
	for i := range extract.Existing {
		extract.Existing[i] = validRecord()
		extract.Existing[i].ID = "r" + strings.Repeat("x", i)
	}
	if err := ValidateExtractRequest(extract); err != nil {
		t.Fatalf("exact baseline: %v", err)
	}
	extract.Existing = append(extract.Existing, validRecord())
	if err := ValidateExtractRequest(extract); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("baseline one-over = %v", err)
	}
}

func TestValidateManagerAndStoreRequests(t *testing.T) {
	remember := RememberRequest{Scope: validScope(), Kind: "preference", Key: "key", Text: "text", Confidence: 1, Source: Provenance{Origin: OriginHuman}}
	if err := ValidateRememberRequest(remember); err != nil {
		t.Fatal(err)
	}
	zeroSource := remember
	zeroSource.Source = Provenance{}
	if err := ValidateRememberRequest(zeroSource); err != nil {
		t.Fatalf("zero manager provenance: %v", err)
	}
	for _, mutate := range []func(*RememberRequest){
		func(r *RememberRequest) { r.ID = "id" }, func(r *RememberRequest) { z := uint64(0); r.ExpectedRevision = &z },
		func(r *RememberRequest) { r.Source.Origin = OriginModel },
	} {
		r := remember
		mutate(&r)
		if err := ValidateRememberRequest(r); !errors.Is(err, ErrInvalidRequest) {
			t.Errorf("remember = %v", err)
		}
	}
	revision := uint64(2)
	remember.ID, remember.ExpectedRevision = "r-1", &revision
	if err := ValidateRememberRequest(remember); err != nil {
		t.Errorf("update remember: %v", err)
	}
	remember.ID, remember.Key = "", ""
	if err := ValidateRememberRequest(remember); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("unidentified update = %v", err)
	}

	propose := ProposeRequest{Action: CandidateCreate, Scope: validScope(), Kind: "fact", Text: "text", Confidence: 1, Source: Provenance{Origin: OriginModel}}
	if err := ValidateProposeRequest(propose); err != nil {
		t.Fatal(err)
	}
	tooLongReason := propose
	tooLongReason.Reason = strings.Repeat("r", MaxReasonBytes+1)
	assertSafeError(t, ValidateProposeRequest(tooLongReason), ErrInvalidRequest, tooLongReason.Reason)
	decidedProposal := propose
	decidedProposal.Source.DecisionAt, decidedProposal.Source.DecisionSource = ptrTime(testNow), OriginHuman
	if err := ValidateProposeRequest(decidedProposal); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("decided pending proposal = %v", err)
	}
	propose.Source.Origin = OriginHuman
	if err := ValidateProposeRequest(propose); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("human proposal = %v", err)
	}

	forget := ForgetRequest{Ref: RecordRef{Scope: validScope(), ID: "r-1"}, ExpectedRevision: 1, PurgeBackups: true}
	if err := ValidateForgetRequest(forget); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("unconfirmed purge = %v", err)
	}
	forget.ConfirmPurge = true
	if err := ValidateForgetRequest(forget); err != nil {
		t.Fatal(err)
	}

	upsert := UpsertRequest{Record: validRecord()}
	if err := ValidateUpsertRequest(upsert); err != nil {
		t.Fatal(err)
	}
	badRev := uint64(0)
	upsert.ExpectedRevision = &badRev
	if err := ValidateUpsertRequest(upsert); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("upsert = %v", err)
	}
}

func TestValidateReviewRebaseAndEditRules(t *testing.T) {
	base := StoreReviewRequest{Ref: CandidateRef{Scope: validScope(), ID: "c-1"}, ResultRecordID: "r-new", Decision: ReviewAccept, DecisionSource: OriginHuman, DecidedAt: testNow}
	if err := ValidateStoreReviewRequest(base, validCandidate()); err != nil {
		t.Fatalf("accept create: %v", err)
	}
	bad := base
	bad.ResultRecordID = ""
	if err := ValidateStoreReviewRequest(bad, validCandidate()); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("missing generated ID = %v", err)
	}
	bad = base
	bad.Decision = ReviewDecision("unknown")
	if err := ValidateStoreReviewRequest(bad, validCandidate()); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("unknown review decision = %v", err)
	}
	bad = base
	bad.DecisionSource = OriginModel
	if err := ValidateStoreReviewRequest(bad, validCandidate()); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("model review = %v", err)
	}

	candidate := validCandidate()
	candidate.Action = CandidateUpdate
	candidate.TargetID = "r-1"
	candidate.BaseRevision = 2
	base.ResultRecordID = ""
	target := uint64(3)
	base.TargetRevision = &target
	if err := ValidateStoreReviewRequest(base, candidate); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("blind rebase = %v", err)
	}
	edited := candidate.Proposed
	base.Edited = &edited
	if err := ValidateStoreReviewRequest(base, candidate); err != nil {
		t.Errorf("edited rebase = %v", err)
	}
	base.Decision = ReviewReject
	if err := ValidateStoreReviewRequest(base, candidate); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("edited rejection = %v", err)
	}
}

func TestValidateManagerReviewEditedContent(t *testing.T) {
	edited := validCandidate().Proposed
	request := ReviewRequest{Ref: CandidateRef{Scope: validScope(), ID: "c-1"}, Decision: ReviewAccept, Edited: &edited}
	if err := ValidateReviewRequest(request); err != nil {
		t.Fatalf("valid edit: %v", err)
	}
	request.Edited.ID = "persisted-id"
	if err := ValidateReviewRequest(request); !errors.Is(err, ErrInvalidRequest) && !errors.Is(err, ErrInvalidRecord) {
		t.Errorf("persisted edit = %v", err)
	}
	request.Edited.ID = ""
	request.Edited.Scope = Scope{Namespace: NamespaceWorkspace, ID: "w-1"}
	if err := ValidateReviewRequest(request); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("wrong edit scope = %v", err)
	}
}

func TestValidateObservationAndBatchBoundaries(t *testing.T) {
	obs := Observation{ID: "o-1", UserText: strings.Repeat("u", MaxObservationBytes), SessionID: "s-1"}
	if err := ValidateObservation(obs); err != nil {
		t.Fatalf("exact observation bytes: %v", err)
	}
	obs.UserText += "x"
	assertSafeError(t, ValidateObservation(obs), ErrInvalidRequest, obs.UserText)

	for _, fact := range []ToolFact{
		{ToolName: strings.Repeat("n", MaxToolNameBytes), Text: "f"},
		{ToolName: "n", Text: strings.Repeat("f", MaxToolFactTextBytes)},
	} {
		if err := ValidateObservation(Observation{ID: "o-1", ToolFacts: []ToolFact{fact}}); err != nil {
			t.Errorf("exact tool fact boundary: %v", err)
		}
	}

	obs = Observation{ID: "o-1", SessionID: "s-1", MessageIDs: make([]string, MaxObservationMessageIDs), ToolFacts: make([]ToolFact, MaxToolFacts)}
	for i := range obs.MessageIDs {
		obs.MessageIDs[i] = "m"
	}
	for i := range obs.ToolFacts {
		obs.ToolFacts[i] = ToolFact{ToolName: "n", Text: "f"}
	}
	if err := ValidateObservation(obs); err != nil {
		t.Fatalf("exact observation counts: %v", err)
	}
	obs.MessageIDs = append(obs.MessageIDs, "over")
	if err := ValidateObservation(obs); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("message count = %v", err)
	}

	batch := ProposalBatch{Candidates: make([]Candidate, MaxCandidateBatch)}
	for i := range batch.Candidates {
		batch.Candidates[i] = validCandidate()
		batch.Candidates[i].ID = "c-" + strings.Repeat("x", i)
	}
	if err := ValidateProposalBatch(batch); err != nil {
		t.Fatalf("exact batch: %v", err)
	}
	batch.Candidates = append(batch.Candidates, validCandidate())
	if err := ValidateProposalBatch(batch); !errors.Is(err, ErrInvalidRequest) {
		t.Errorf("batch count = %v", err)
	}
}

func ptrTime(v time.Time) *time.Time { return &v }
