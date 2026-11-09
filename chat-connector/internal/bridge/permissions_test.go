package bridge

import (
	"context"
	"github.com/baiyuqing/otto/chat-connector/internal/agent"
	"github.com/baiyuqing/otto/chat-connector/internal/agent/fake"
	"strings"
	"testing"
)

func TestPermissionsKeepsPolicyAndReviewerIndependent(t *testing.T) {
	h := newHarness(t, setup{env: []string{"FAKE_APPROVAL_POLICY=1"}})
	for i, command := range []string{"/permissions", "/permissions policy never", "/permissions reviewer user", "/permissions policy on_request"} {
		h.say(command)
		texts := h.waitSent(i + 1)
		text := texts[i]
		want := []string{"Approval policy: AUTO", "Approval policy: NEVER", "Reviewer: user (inactive)", "Approval policy: MANUAL"}[i]
		if !strings.Contains(text, want) {
			t.Fatalf("%s = %q, want %q", command, text, want)
		}
	}
	sid := h.store.Session("fake:c1")
	policy, err := h.agent.ApprovalPolicy(context.Background(), sid)
	if err != nil || policy.Policy != "on_request" || policy.Reviewer != "user" {
		t.Fatalf("policy=%+v err=%v", policy, err)
	}
	h.say("/permissions reviewer bypass")
	if text := h.waitSent(5)[4]; !strings.Contains(text, "Usage: /permissions") {
		t.Fatal(text)
	}
	if calls := strings.Join(h.calls("start:"), "\n"); strings.Contains(calls, "/permissions") {
		t.Fatal("policy command forwarded to model")
	}
}

func TestPermissionsLegacyInspectionAndMutationRefusal(t *testing.T) {
	h := newHarness(t, setup{})
	h.say("/permissions")
	if text := h.waitSent(1)[0]; !strings.Contains(text, "Legacy agent") {
		t.Fatal(text)
	}
	h.say("/permissions policy never")
	if text := h.waitSent(2)[1]; !strings.Contains(text, "upgrade Otto") {
		t.Fatal(text)
	}
	if calls := h.calls("auto:"); len(calls) != 0 {
		t.Fatalf("unsupported change mutated preset: %v", calls)
	}
}

func TestReviewStatesNeverShowActionableCardsBeforeHumanRequired(t *testing.T) {
	h := newHarness(t, setup{})
	c := h.b.chat(h.plat, "c1")
	request := agent.SharedApproval{ID: "reviewed", Revision: 1, State: "pending", Title: "Action", Blocking: true, PresentationOrigin: c.presentationOrigin()}
	for _, status := range []string{"queued", "reviewing", "completed"} {
		request.Review = &agent.ReviewMetadata{Status: status}
		c.showApprovals(context.Background(), "sid", []agent.SharedApproval{request})
	}
	if got := h.plat.texts(); len(got) != 0 {
		t.Fatalf("automatic review showed buttons/details: %v", got)
	}
	c.mu.Lock()
	blocking := c.sharedBlocking
	c.mu.Unlock()
	if blocking {
		t.Fatal("nonhuman review blocked chat dialogue")
	}
	request.Review.Status = "human_required"
	c.showApprovals(context.Background(), "sid", []agent.SharedApproval{request})
	if len(h.plat.texts()) != 2 {
		t.Fatal("human review missing card")
	}
	c.mu.Lock()
	oldToken := c.sharedCards[request.ID].token
	c.mu.Unlock()
	request.Revision++
	c.showApprovals(context.Background(), "sid", []agent.SharedApproval{request})
	c.mu.Lock()
	fresh := c.sharedCards[request.ID]
	c.mu.Unlock()
	if fresh.token == oldToken || fresh.request.Revision != 2 {
		t.Fatal("changed revision reused callback")
	}
	if c.decideSharedCard(Message{ApprovalID: oldToken, Text: "/allow"}) {
		t.Fatal("old callback admitted new revision")
	}
	if h.b.chat(h.plat, "other").decideSharedCard(Message{ApprovalID: fresh.token, Text: "/allow"}) {
		t.Fatal("callback crossed chat")
	}
	request.State = "rejected"
	c.showApprovals(context.Background(), "sid", []agent.SharedApproval{request})
	if c.decideSharedCard(Message{ApprovalID: fresh.token, Text: "/allow"}) {
		t.Fatal("settled callback remained actionable")
	}
}

func TestAutomaticReviewAuditDoesNotPostChatMessages(t *testing.T) {
	h := newHarness(t, setup{})
	c := h.b.chat(h.plat, "c1")
	request := agent.SharedApproval{ID: "r", Title: "Action", State: "pending", Review: &agent.ReviewMetadata{Status: "reviewing"}}
	c.reviewNotice(context.Background(), request)
	request.State, request.Application = "rejected", "not_required"
	request.Review.Status = "completed"
	request.Review.Assessment = &agent.ReviewAssessment{Risk: "critical", Outcome: "deny", Reason: "Outside authorized scope"}
	c.reviewNotice(context.Background(), request)
	request.State, request.Review.Assessment.Outcome = "approved", "allow"
	c.reviewNotice(context.Background(), request)
	if len(h.plat.texts()) != 0 {
		t.Fatal("automatic review flooded chat")
	}
	request.Application, request.ApplicationError = "unknown", "Transport lost during application"
	c.reviewNotice(context.Background(), request)
	texts := h.plat.texts()
	if len(texts) != 1 || !strings.Contains(texts[0], "application unknown") || !strings.Contains(texts[0], "Transport lost") {
		t.Fatalf("missing application failure reminder: %v", texts)
	}
	if c.decideSharedCard(Message{ApprovalID: "r", Text: "/allow"}) {
		t.Fatal("passive notice was actionable")
	}
}

func TestApprovalCountsSeparateReviewHumanAndApplicationFailure(t *testing.T) {
	h := newHarness(t, setup{})
	c := h.b.chat(h.plat, "c1")
	requests := []agent.SharedApproval{
		{ID: "human", State: "pending"},
		{ID: "review", State: "pending", Review: &agent.ReviewMetadata{Status: "reviewing"}},
		{ID: "failed", State: "approved", Application: "unknown", Review: &agent.ReviewMetadata{Status: "completed"}},
	}
	for _, r := range requests {
		r.PresentationOrigin = c.presentationOrigin()
		c.recordApprovalStatus("sid", r)
	}
	if counts := c.approvalCounts("sid"); !strings.Contains(counts, "Human pending: 1 · Reviewing: 1 · Application failures: 1") {
		t.Fatal(counts)
	}
	requests[1].State = "approved"
	requests[1].PresentationOrigin = c.presentationOrigin()
	c.recordApprovalStatus("sid", requests[1])
	if counts := c.approvalCounts("sid"); !strings.Contains(counts, "Reviewing: 0") {
		t.Fatal(counts)
	}
	if counts := c.approvalCounts("other-session"); counts != "" {
		t.Fatal("counts crossed session")
	}
}

func TestOngoingPromptGetsHumanCardWhenReviewStatusChanges(t *testing.T) {
	h := newHarness(t, setup{})
	h.say("wait")
	h.waitCall("start:wait")
	c := h.b.chat(h.plat, "c1")
	sid := h.store.Session("fake:c1")
	r := agent.SharedApproval{
		ID: "same-request", Revision: 1, State: "pending", Title: "Action", Blocking: true,
		PresentationOrigin: c.presentationOrigin(), Review: &agent.ReviewMetadata{Status: "queued"},
	}
	for _, status := range []string{"queued", "reviewing"} {
		r.Review.Status = status
		c.showApprovals(context.Background(), sid, []agent.SharedApproval{r})
	}
	if len(h.plat.texts()) != 0 {
		t.Fatal("review offered an actionable card")
	}
	r.Review.Status = "human_required"
	// Same pending state, ID and revision: the review disposition alone changes.
	c.showApprovals(context.Background(), sid, []agent.SharedApproval{r})
	c.mu.Lock()
	active, card := c.taskControlPhaseLocked() == taskControlActive, c.sharedCards[r.ID]
	c.mu.Unlock()
	if !active || card == nil || card.session != sid || card.request.Revision != 1 || len(h.plat.texts()) != 2 {
		t.Fatalf("active=%v card=%+v messages=%v", active, card, h.plat.texts())
	}
	other := h.b.chat(h.plat, "other")
	other.showApprovals(context.Background(), sid, []agent.SharedApproval{r})
	if other.decideSharedCard(Message{ApprovalID: card.token, Text: "/allow"}) {
		t.Fatal("live card crossed origin")
	}
	if err := fake.Release(h.dir); err != nil {
		t.Fatal(err)
	}
	h.waitCall("end:wait")
}

func TestSharedCallbackIdentitySurvivesRecreationAndBindsRevision(t *testing.T) {
	h := newHarness(t, setup{})
	c := h.b.chat(h.plat, "c1")
	r := agent.SharedApproval{ID: "durable-request", Revision: 3, State: "pending", PresentationOrigin: c.presentationOrigin()}
	r.Subject.Version = "version1"
	c.showApprovals(context.Background(), "sid", []agent.SharedApproval{r})
	c.mu.Lock()
	old := c.sharedCards[r.ID].token
	delete(c.sharedCards, r.ID)
	c.mu.Unlock()
	c.showApprovals(context.Background(), "sid", []agent.SharedApproval{r})
	c.mu.Lock()
	card := c.sharedCards[r.ID]
	card.deciding = true
	c.mu.Unlock()
	if card.token != old {
		t.Fatal("unchanged durable request got a different callback identity")
	}
	if c.sharedApprovalToken("other-session", r) == old || h.b.chat(h.plat, "other").sharedApprovalToken("sid", r) == old {
		t.Fatal("callback identity did not bind origin/session")
	}
	r.Revision++
	if c.sharedApprovalToken("sid", r) == old {
		t.Fatal("callback identity did not bind revision")
	}
	r.Revision--
	r.Subject.Version = "version2"
	if c.sharedApprovalToken("sid", r) == old {
		t.Fatal("callback identity did not bind subject version")
	}
	before := len(h.plat.texts())
	if !c.cmdSharedDecision(Message{Text: "/allow"}, true, old+" feedback") {
		t.Fatal("exact callback token was not accepted by text command")
	}
	if len(h.plat.texts()) != before {
		t.Fatal("exact callback token was not resolved to its existing approval")
	}
	c.mu.Lock()
	card.deciding = false
	c.mu.Unlock()
	c.showApprovals(context.Background(), "sid", []agent.SharedApproval{r})
	if c.decideSharedCard(Message{ApprovalID: old, Text: "/allow"}) {
		t.Fatal("old subject-version callback remained actionable")
	}
}
