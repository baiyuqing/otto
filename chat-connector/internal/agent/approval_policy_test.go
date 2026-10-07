package agent

import (
	"context"
	"encoding/json"
	"errors"
	"testing"
)

func TestApprovalPolicyCanonicalMutationAndStaleRevision(t *testing.T) {
	a, _, _ := fakeAgent(t, "FAKE_APPROVAL_POLICY=1")
	ctx := context.Background()
	sid, err := a.New(ctx)
	if err != nil {
		t.Fatal(err)
	}
	initial, err := a.ApprovalPolicy(ctx, sid)
	if err != nil || initial.Revision != 1 || initial.Policy != "on_request" || initial.Reviewer != "auto_review" || !initial.ReviewerActive || initial.Legacy {
		t.Fatalf("initial=%+v err=%v", initial, err)
	}
	never, err := a.SetApprovalPolicy(ctx, sid, initial.Revision, "never", "user")
	if err != nil || never.Revision != 2 || never.Mode != "auto" || !never.Automatic || never.ReviewerActive {
		t.Fatalf("never=%+v err=%v", never, err)
	}
	if _, err := a.SetApprovalPolicy(ctx, sid, initial.Revision, "on_request", "user"); err == nil {
		t.Fatal("stale settings mutation succeeded")
	}
	after, err := a.ApprovalPolicy(ctx, sid)
	if err != nil || *after != *never {
		t.Fatalf("stale mutation changed policy: %+v err=%v", after, err)
	}
	auto := true
	if _, err := a.ApprovalMode(ctx, sid, &auto); err != nil {
		t.Fatal(err)
	}
	after, err = a.ApprovalPolicy(ctx, sid)
	if err != nil || after.Policy != "on_request" || after.Reviewer != "auto_review" || after.Revision != 3 {
		t.Fatalf("preset did not reset both controls: %+v err=%v", after, err)
	}
}

func TestApprovalPolicyLegacyInspectNeverFakesMutation(t *testing.T) {
	a, _, _ := fakeAgent(t)
	ctx := context.Background()
	sid, err := a.New(ctx)
	if err != nil {
		t.Fatal(err)
	}
	old, err := a.ApprovalPolicy(ctx, sid)
	if err != nil || !old.Legacy || old.Mode != "auto" || old.Revision != 0 {
		t.Fatalf("legacy=%+v err=%v", old, err)
	}
	if _, err := a.SetApprovalPolicy(ctx, sid, 1, "never", "user"); !errors.Is(err, ErrApprovalPolicyUnsupported) {
		t.Fatalf("error=%v", err)
	}
	after, err := a.ApprovalPolicy(ctx, sid)
	if err != nil || *old != *after {
		t.Fatalf("unsupported setter changed legacy mode: %+v err=%v", after, err)
	}
}

func TestApprovalPolicyAdvertRequiresVersionOne(t *testing.T) {
	for _, value := range []any{nil, true, map[string]any{}, map[string]any{"version": 2}, map[string]any{"version": "1"}} {
		if advertisesApprovalPolicy(map[string]any{"otto": map[string]any{"approvalPolicy": value}}) {
			t.Fatalf("accepted invalid advert %#v", value)
		}
	}
	if !advertisesApprovalPolicy(map[string]any{"otto": map[string]any{"approvalPolicy": map[string]any{"version": float64(1)}}}) {
		t.Fatal("version1 not recognized")
	}
}

func TestApprovalPolicyDecodeRejectsIncompleteAndInconsistentResponses(t *testing.T) {
	for _, raw := range []string{
		`{"policy":"on_request","reviewer":"auto_review","revision":1,"mode":"auto"}`,
		`{"policy":"never","reviewer":"auto_review","revision":1,"mode":"auto","automatic":true,"reviewer_active":true}`,
		`{"policy":"on_request","reviewer":"user","revision":1,"mode":"auto","automatic":true,"reviewer_active":false}`,
	} {
		if _, err := decodeApprovalPolicy([]byte(raw)); err == nil {
			t.Fatalf("invalid response accepted: %s", raw)
		}
	}
}

func TestSharedApprovalHumanPendingRequiresReviewDisposition(t *testing.T) {
	for _, status := range []string{"queued", "reviewing", "human_required", "completed", "unknown"} {
		raw := []byte(`{"state":"pending","review":{"status":"` + status + `","settings_revision":3,"evidence_digest":"e","policy_digest":"p","assessment":null}}`)
		var request SharedApproval
		if err := json.Unmarshal(raw, &request); err != nil {
			t.Fatal(err)
		}
		if request.HumanPending() != (status == "human_required") {
			t.Fatalf("status=%s pending=%v", status, request.HumanPending())
		}
	}
	if !(SharedApproval{State: "pending"}).HumanPending() {
		t.Fatal("legacy human request hidden")
	}
	if (SharedApproval{State: "rejected", Review: &ReviewMetadata{Status: "human_required"}}).HumanPending() {
		t.Fatal("settled request presented")
	}
}
