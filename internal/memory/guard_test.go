package memory

import (
	"context"
	"errors"
	"fmt"
	"reflect"
	"strings"
	"sync"
	"testing"
)

func checkGuard(t *testing.T, guard ContentGuard, field GuardField) error {
	t.Helper()
	return guard.Check(context.Background(), GuardInput{Fields: []GuardField{field}})
}

func TestDefaultGuardRejectsSensitiveShapesWithoutEcho(t *testing.T) {
	guard := DefaultGuard{}
	cases := []string{
		"Authorization: Bearer [REDACTED]",
		"Cookie: session=[REDACTED]",
		"https://user:[REDACTED]@example.invalid/path",
		"-----BEGIN PRIVATE KEY-----",
		"-----END RSA PRIVATE KEY-----",
		"api_key=[REDACTED]",
		"access-token: [REDACTED]",
		"password = [REDACTED]",
		"prefix [REDACTED] suffix",
	}
	for _, sample := range cases {
		t.Run(fmt.Sprintf("shape-%d", len(sample)), func(t *testing.T) {
			err := checkGuard(t, guard, GuardField{Name: "text", Value: sample})
			assertSafeError(t, err, ErrSensitiveMemory, sample)
		})
	}
}

func TestDefaultGuardScansMarkerInEverySemanticAndOpaqueClass(t *testing.T) {
	guard := DefaultGuard{}
	for _, field := range []GuardField{
		{Name: "text", Value: "[REDACTED]"}, {Name: "key", Value: "[REDACTED]"},
		{Name: "label", Value: "[REDACTED]"}, {Name: "metadata", Value: "[REDACTED]"},
		{Name: "reason", Value: "[REDACTED]"}, {Name: "record ID", Value: "[REDACTED]", Opaque: true},
	} {
		if err := checkGuard(t, guard, field); !errors.Is(err, ErrSensitiveMemory) {
			t.Errorf("%s: %v", field.Name, err)
		}
	}
}

func TestDefaultGuardAllowsOrdinaryContentAndSkipsOpaqueTokenHeuristics(t *testing.T) {
	guard := DefaultGuard{}
	allowed := []GuardField{
		{Name: "code", Value: `func tokenization(input string) int { return len(input) }`},
		{Name: "url", Value: "https://example.invalid/path?q=tokenization"},
		{Name: "metadata", Value: "token_budget=2000"},
		{Name: "prose", Value: "Tokenization can reduce a token budget."},
		{Name: "opaque ID", Value: "api_key=synthetic-placeholder", Opaque: true},
	}
	for _, field := range allowed {
		if err := checkGuard(t, guard, field); err != nil {
			t.Errorf("%s rejected: %v", field.Name, err)
		}
	}
}

func TestDefaultGuardInputBoundsFailClosed(t *testing.T) {
	guard := DefaultGuard{}
	fields := make([]GuardField, MaxGuardFields)
	if err := guard.Check(context.Background(), GuardInput{Fields: fields}); err != nil {
		t.Fatalf("exact field count: %v", err)
	}
	fields = append(fields, GuardField{})
	assertSafeError(t, guard.Check(context.Background(), GuardInput{Fields: fields}), ErrSensitiveMemory)

	exact := strings.Repeat("x", MaxGuardBytes)
	if err := checkGuard(t, guard, GuardField{Name: "text", Value: exact}); err != nil {
		t.Fatalf("exact byte count: %v", err)
	}
	over := exact + "X"
	assertSafeError(t, checkGuard(t, guard, GuardField{Name: "text", Value: over}), ErrSensitiveMemory, over)
}

func TestDefaultGuardCancellationAndInvalidText(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if err := (DefaultGuard{}).Check(ctx, GuardInput{}); !errors.Is(err, context.Canceled) {
		t.Fatalf("canceled = %v", err)
	}
	unsafe := "LEAK_" + string([]byte{0xff})
	assertSafeError(t, checkGuard(t, DefaultGuard{}, GuardField{Name: "text", Value: unsafe}), ErrSensitiveMemory, unsafe)
}

func TestExactGuardConstructionBoundsHashesAndDoesNotRetainValues(t *testing.T) {
	values := []string{"SYNTHETIC_SECRET_42", "https://resolved.example.invalid/endpoint"}
	guard, err := NewExactGuard(values)
	if err != nil {
		t.Fatal(err)
	}
	values[0] = "changed-after-construction"
	if err := checkGuard(t, guard, GuardField{Name: "text", Value: "SYNTHETIC_SECRET_42"}); !errors.Is(err, ErrSensitiveMemory) {
		t.Fatalf("copied exact value = %v", err)
	}
	if strings.Contains(fmt.Sprintf("%#v", guard), "SYNTHETIC_SECRET_42") {
		t.Fatal("ExactGuard retained raw configured value")
	}
	if hasStringValue(reflect.ValueOf(guard), "SYNTHETIC_SECRET_42") {
		t.Fatal("ExactGuard contains raw configured value")
	}

	exact := strings.Repeat("s", MaxExactGuardValueBytes)
	if _, err := NewExactGuard([]string{exact}); err != nil {
		t.Fatalf("exact value bytes: %v", err)
	}
	over := exact + "s"
	_, err = NewExactGuard([]string{over})
	assertSafeError(t, err, ErrInvalidRequest, over)
	many := make([]string, MaxExactGuardValues)
	for i := range many {
		many[i] = fmt.Sprintf("synthetic-%d", i)
	}
	if _, err = NewExactGuard(many); err != nil {
		t.Fatalf("exact value count: %v", err)
	}
	many = append(many, "synthetic-over")
	_, err = NewExactGuard(many)
	assertSafeError(t, err, ErrInvalidRequest)
}

func hasStringValue(v reflect.Value, target string) bool {
	if !v.IsValid() {
		return false
	}
	if v.Kind() == reflect.Pointer || v.Kind() == reflect.Interface {
		if v.IsNil() {
			return false
		}
		return hasStringValue(v.Elem(), target)
	}
	switch v.Kind() {
	case reflect.String:
		return v.String() == target
	case reflect.Struct:
		for i := 0; i < v.NumField(); i++ {
			if hasStringValue(v.Field(i), target) {
				return true
			}
		}
	case reflect.Slice, reflect.Array:
		for i := 0; i < v.Len(); i++ {
			if hasStringValue(v.Index(i), target) {
				return true
			}
		}
	case reflect.Map:
		for _, key := range v.MapKeys() {
			if hasStringValue(key, target) || hasStringValue(v.MapIndex(key), target) {
				return true
			}
		}
	}
	return false
}

func TestExactGuardScansWholeLexicalHeaderURIAndOpaqueSpans(t *testing.T) {
	secret := "SYNTHETIC_SECRET_42"
	endpoint := "https://resolved.example.invalid/endpoint"
	guard, err := NewExactGuard([]string{secret, endpoint, "header-synthetic-value"})
	if err != nil {
		t.Fatal(err)
	}
	cases := []GuardField{
		{Name: "whole", Value: secret},
		{Name: "lexical", Value: "safe prose embeds " + secret + " here"},
		{Name: "header", Value: "Authorization: header-synthetic-value"},
		{Name: "URI", Value: "resolved URL is " + endpoint + " for this run"},
		{Name: "opaque", Value: "prefix " + secret + " suffix", Opaque: true},
	}
	for _, field := range cases {
		err := checkGuard(t, guard, field)
		assertSafeError(t, err, ErrSensitiveMemory, secret, endpoint, "header-synthetic-value")
	}
	if err := checkGuard(t, guard, GuardField{Name: "url", Value: "https://ordinary.example.invalid/endpoint"}); err != nil {
		t.Fatalf("ordinary URL: %v", err)
	}
}

func TestExactGuardSpanOverflowFailsClosed(t *testing.T) {
	guard, err := NewExactGuard([]string{"not-present-synthetic-value"})
	if err != nil {
		t.Fatal(err)
	}
	exact := strings.Repeat("x ", MaxExactGuardSpans-1)
	if err := checkGuard(t, guard, GuardField{Name: "text", Value: exact}); err != nil {
		t.Fatalf("exact span count: %v", err)
	}
	input := strings.Repeat("x ", MaxExactGuardSpans+1)
	assertSafeError(t, checkGuard(t, guard, GuardField{Name: "text", Value: input}), ErrSensitiveMemory, input)
}

type testGuard func(context.Context, GuardInput) error

func (g testGuard) Check(ctx context.Context, input GuardInput) error { return g(ctx, input) }

func TestCompositeGuardCopiesMembersFailsClosedAndSanitizesErrors(t *testing.T) {
	firstCalls, secondCalls := 0, 0
	members := []ContentGuard{
		testGuard(func(context.Context, GuardInput) error { firstCalls++; return nil }),
		testGuard(func(context.Context, GuardInput) error { secondCalls++; return ErrSensitiveMemory }),
	}
	composite := NewCompositeGuard(members...)
	members[0] = testGuard(func(context.Context, GuardInput) error { return errors.New("LEAK_replaced") })
	err := composite.Check(context.Background(), GuardInput{})
	if !errors.Is(err, ErrSensitiveMemory) || firstCalls != 1 || secondCalls != 1 {
		t.Fatalf("copy/fail closed: err=%v calls=%d,%d", err, firstCalls, secondCalls)
	}

	unknown := NewCompositeGuard(testGuard(func(context.Context, GuardInput) error { return errors.New("LEAK_unknown_member_error") }))
	err = unknown.Check(context.Background(), GuardInput{})
	if err != ErrUnavailable || strings.Contains(err.Error(), "LEAK") {
		t.Fatalf("unknown error not sanitized: %v", err)
	}
	unavailable := NewCompositeGuard(testGuard(func(context.Context, GuardInput) error { return fmt.Errorf("unsafe wrapper: %w", ErrUnavailable) }))
	if err := unavailable.Check(context.Background(), GuardInput{}); err != ErrUnavailable {
		t.Fatalf("unavailable = %v", err)
	}
	if err := NewCompositeGuard(nil).Check(context.Background(), GuardInput{}); err != ErrUnavailable {
		t.Fatalf("nil member = %v", err)
	}
}

func TestCompositeGuardPreservesContextAndIsConcurrent(t *testing.T) {
	composite := NewCompositeGuard(DefaultGuard{}, mustExactGuard(t, []string{"SYNTHETIC_SECRET_42"}))
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if err := composite.Check(ctx, GuardInput{}); !errors.Is(err, context.Canceled) {
		t.Fatalf("context = %v", err)
	}

	var wg sync.WaitGroup
	for i := 0; i < 100; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			_ = composite.Check(context.Background(), GuardInput{Fields: []GuardField{{Name: "text", Value: "ordinary code"}}})
		}()
	}
	wg.Wait()
}

func mustExactGuard(t *testing.T, values []string) *ExactGuard {
	t.Helper()
	guard, err := NewExactGuard(values)
	if err != nil {
		t.Fatal(err)
	}
	return guard
}

func TestGuardRecordCoversEveryPersistedCallerControlledString(t *testing.T) {
	base := validRecord()
	mutations := []func(*Record){
		func(r *Record) { r.ID = "[REDACTED]" }, func(r *Record) { r.Scope.Namespace = "[REDACTED]" },
		func(r *Record) { r.Scope.ID = "[REDACTED]" }, func(r *Record) { r.Kind = "[REDACTED]" },
		func(r *Record) { r.Key = "[REDACTED]" }, func(r *Record) { r.Text = "[REDACTED]" },
		func(r *Record) { r.Labels[0] = "[REDACTED]" }, func(r *Record) { r.Metadata = map[string]string{"[REDACTED]": "safe"} },
		func(r *Record) { r.Metadata = map[string]string{"safe": "[REDACTED]"} },
		func(r *Record) { r.Source.SessionID = "[REDACTED]" }, func(r *Record) { r.Source.MessageIDs[0] = "[REDACTED]" },
		func(r *Record) { r.Source.ObservationID = "[REDACTED]" },
	}
	for i, mutate := range mutations {
		r := CloneRecord(base)
		mutate(&r)
		if err := GuardRecord(context.Background(), DefaultGuard{}, r); !errors.Is(err, ErrSensitiveMemory) {
			t.Errorf("field %d not guarded: %v", i, err)
		}
	}
}

func TestGuardCandidateObservationCommitAndReceiptCoverPersistedStrings(t *testing.T) {
	c := validCandidate()
	c.Reason = "[REDACTED]"
	if err := GuardCandidate(context.Background(), DefaultGuard{}, c); !errors.Is(err, ErrSensitiveMemory) {
		t.Errorf("candidate reason: %v", err)
	}
	c = validCandidate()
	c.TargetID = "[REDACTED]"
	if err := GuardCandidate(context.Background(), DefaultGuard{}, c); !errors.Is(err, ErrSensitiveMemory) {
		t.Errorf("candidate target: %v", err)
	}

	obs := Observation{ID: "o", UserText: "safe", AssistantText: "safe", SessionID: "s", ToolFacts: []ToolFact{{ToolName: "tool", Text: "[REDACTED]"}}, MessageIDs: []string{"m"}}
	if err := GuardObservation(context.Background(), DefaultGuard{}, obs); !errors.Is(err, ErrSensitiveMemory) {
		t.Errorf("observation fact: %v", err)
	}

	commit := ObservationCommit{ObservationID: "[REDACTED]", Candidates: []Candidate{validCandidate()}}
	if err := GuardObservationCommit(context.Background(), DefaultGuard{}, commit); !errors.Is(err, ErrSensitiveMemory) {
		t.Errorf("commit: %v", err)
	}
	receipt := ObservationReceipt{ObservationID: "safe", CandidateIDs: []string{"[REDACTED]"}}
	if err := GuardObservationReceipt(context.Background(), DefaultGuard{}, receipt); !errors.Is(err, ErrSensitiveMemory) {
		t.Errorf("receipt: %v", err)
	}
}

func TestGuardHelpersBoundBeforeCallingGuardAndDoNotMutate(t *testing.T) {
	r := validRecord()
	r.Labels = make([]string, MaxGuardFields+1)
	called := false
	guard := testGuard(func(context.Context, GuardInput) error { called = true; return nil })
	before := CloneRecord(r)
	assertSafeError(t, GuardRecord(context.Background(), guard, r), ErrSensitiveMemory)
	if called {
		t.Fatal("guard called with oversized input")
	}
	if !reflect.DeepEqual(r, before) {
		t.Fatal("guard helper mutated input")
	}
	if err := GuardRecord(context.Background(), nil, validRecord()); err != ErrUnavailable {
		t.Fatalf("nil guard = %v", err)
	}
}
