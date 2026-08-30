package sqlite

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
)

const processHelperEnvironment = "OTTO_SQLITE_PROCESS_HELPER"

func TestProcessHelper(t *testing.T) {
	if os.Getenv(processHelperEnvironment) != "1" {
		return
	}
	childID := os.Getenv("OTTO_SQLITE_CHILD_ID")
	fmt.Printf("ready %s\n", childID)
	if _, err := bufio.NewReader(os.Stdin).ReadString('\n'); err != nil {
		fmt.Printf("failed %s\n", childID)
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	factory := NewFactory(os.Getenv("OTTO_SQLITE_PATH"), Options{Guard: memory.NewCompositeGuard(memory.DefaultGuard{}), NewID: memory.NewID})
	var components memory.Components
	var err error
	for attempt := 0; attempt < 8; attempt++ {
		components, err = factory.Open(ctx)
		if !errors.Is(err, memory.ErrBusy) {
			break
		}
	}
	if err != nil {
		printProcessResult(classifyProcessError(err), childID)
		return
	}
	store := components.Store
	defer store.Close()
	fmt.Printf("opened %s\n", childID)
	if _, err := bufio.NewReader(os.Stdin).ReadString('\n'); err != nil {
		fmt.Printf("failed %s\n", childID)
		return
	}

	action := os.Getenv("OTTO_SQLITE_ACTION")
	entityID := os.Getenv("OTTO_SQLITE_ENTITY_ID")
	scope := memory.Scope{Namespace: memory.NamespaceUser, ID: os.Getenv("OTTO_SQLITE_SCOPE_ID")}
	key := os.Getenv("OTTO_SQLITE_KEY")
	switch action {
	case "init":
		identity, err := store.Identity(ctx)
		if err != nil {
			printProcessResult(classifyProcessError(err), childID)
			return
		}
		printProcessResult("ok", identity.DatabaseID)
	case "create", "create-exit":
		for attempt := 0; attempt < 8; attempt++ {
			_, err = store.Upsert(ctx, memory.UpsertRequest{Record: processRecord(entityID, scope, key)})
			if !errors.Is(err, memory.ErrBusy) {
				break
			}
		}
		if err != nil {
			printProcessResult(classifyProcessError(err), entityID)
			return
		}
		if action == "create-exit" {
			os.Exit(0)
		}
		printProcessResult("ok", entityID)
	case "update":
		record := processRecord(entityID, scope, key)
		record.Text = "fixed updated value"
		record.Revision = 1
		record.UpdatedAt = record.UpdatedAt.Add(time.Second)
		expected := uint64(1)
		for attempt := 0; attempt < 8; attempt++ {
			_, err = store.Upsert(ctx, memory.UpsertRequest{Record: record, ExpectedRevision: &expected})
			if !errors.Is(err, memory.ErrBusy) {
				break
			}
		}
		printProcessResult(classifyProcessError(err), entityID)
	case "observe":
		observationID := os.Getenv("OTTO_SQLITE_OBSERVATION_ID")
		candidate := processCandidate(entityID, scope)
		candidate.Proposed.Source.ObservationID = observationID
		var receipt memory.ObservationReceipt
		for attempt := 0; attempt < 8; attempt++ {
			receipt, err = store.CommitObservation(ctx, memory.ObservationCommit{ObservationID: observationID, Candidates: []memory.Candidate{candidate}, CreatedAt: processTime().Add(2 * time.Second)})
			if !errors.Is(err, memory.ErrBusy) {
				break
			}
		}
		if err != nil || len(receipt.CandidateIDs) != 1 {
			printProcessResult(classifyProcessError(err), entityID)
			return
		}
		category := "ok"
		if receipt.Existing {
			category = "existing"
		}
		printProcessResult(category, receipt.CandidateIDs[0])
	case "review":
		request := memory.StoreReviewRequest{
			Ref: memory.CandidateRef{Scope: scope, ID: entityID}, Decision: memory.ReviewReject,
			DecisionSource: memory.OriginHuman, DecidedAt: processTime().Add(4 * time.Second),
		}
		for attempt := 0; attempt < 8; attempt++ {
			_, err = store.Review(ctx, request)
			if !errors.Is(err, memory.ErrBusy) {
				break
			}
		}
		printProcessResult(classifyProcessError(err), entityID)
	default:
		printProcessResult("failed", childID)
	}
}

func processTime() time.Time { return time.Date(2026, 8, 30, 2, 0, 0, 0, time.UTC) }

func processRecord(id string, scope memory.Scope, key string) memory.Record {
	at := processTime()
	return memory.Record{
		ID: id, Scope: scope, Kind: "note", Key: key, Text: "fixed process value", Labels: []string{"fixed"},
		Metadata: map[string]string{"class": "fixed"}, Source: memory.Provenance{Origin: memory.OriginHuman},
		Confidence: 1, CreatedAt: at, UpdatedAt: at,
	}
}

func processCandidate(id string, scope memory.Scope) memory.Candidate {
	return memory.Candidate{
		ID: id, Action: memory.CandidateCreate, State: memory.CandidatePending, CreatedAt: processTime().Add(2 * time.Second), Reason: "fixed",
		Proposed: memory.Record{
			Scope: scope, Kind: "note", Key: id, Text: "fixed candidate value", Labels: []string{"fixed"}, Metadata: map[string]string{},
			Source: memory.Provenance{Origin: memory.OriginModel}, Confidence: 1,
		},
	}
}

func classifyProcessError(err error) string {
	switch {
	case err == nil:
		return "ok"
	case errors.Is(err, memory.ErrConflict):
		return "conflict"
	case errors.Is(err, memory.ErrNotFound):
		return "notfound"
	case errors.Is(err, memory.ErrClosed):
		return "closed"
	case errors.Is(err, memory.ErrBusy):
		return "busy"
	case errors.Is(err, memory.ErrCorrupt):
		return "corrupt"
	case errors.Is(err, memory.ErrUnavailable):
		return "unavailable"
	case errors.Is(err, memory.ErrInvalidRequest):
		return "invalid"
	case errors.Is(err, memory.ErrUnsupported):
		return "unsupported"
	case errors.Is(err, memory.ErrCommitUnknown):
		return "unknown"
	default:
		return "failed"
	}
}

func printProcessResult(category, opaqueID string) {
	fmt.Printf("%s %s\n", category, opaqueID)
}

type processChild struct {
	id     string
	cmd    *exec.Cmd
	cancel context.CancelFunc
	stdin  io.WriteCloser
	scan   *bufio.Scanner
}

func startProcessChild(t *testing.T, path, childID, action string, values map[string]string) *processChild {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	command := exec.CommandContext(ctx, os.Args[0], "-test.run=^TestProcessHelper$", "-test.count=1")
	command.Env = append(os.Environ(),
		processHelperEnvironment+"=1",
		"OTTO_SQLITE_PATH="+path,
		"OTTO_SQLITE_CHILD_ID="+childID,
		"OTTO_SQLITE_ACTION="+action,
	)
	for key, value := range values {
		command.Env = append(command.Env, key+"="+value)
	}
	stdin, err := command.StdinPipe()
	if err != nil {
		cancel()
		t.Fatal(err)
	}
	stdout, err := command.StdoutPipe()
	if err != nil {
		cancel()
		t.Fatal(err)
	}
	command.Stderr = io.Discard
	if err := command.Start(); err != nil {
		cancel()
		t.Fatal(err)
	}
	child := &processChild{id: childID, cmd: command, cancel: cancel, stdin: stdin, scan: bufio.NewScanner(stdout)}
	if category, id := child.readResult(t); category != "ready" || id != childID {
		t.Fatalf("helper readiness = %s %s", category, id)
	}
	return child
}

func (child *processChild) release(t *testing.T) {
	t.Helper()
	if _, err := io.WriteString(child.stdin, "release\n"); err != nil {
		t.Fatal(err)
	}
}

func (child *processChild) awaitOpened(t *testing.T) {
	t.Helper()
	category, id := child.readResult(t)
	if category != "opened" || id != child.id {
		t.Fatalf("helper open barrier = %s %s", category, id)
	}
}

func (child *processChild) releaseAction(t *testing.T) {
	t.Helper()
	child.release(t)
	if err := child.stdin.Close(); err != nil {
		t.Fatal(err)
	}
}

func (child *processChild) finish(t *testing.T) (string, string) {
	t.Helper()
	defer child.cancel()
	category, id := child.readResult(t)
	if err := child.cmd.Wait(); err != nil {
		t.Fatalf("helper %s exit category: %v", child.id, err)
	}
	return category, id
}

func (child *processChild) finishWithoutResult(t *testing.T) {
	t.Helper()
	defer child.cancel()
	if child.scan.Scan() {
		t.Fatalf("helper %s unexpectedly responded: %q", child.id, child.scan.Text())
	}
	if err := child.cmd.Wait(); err != nil {
		t.Fatalf("helper %s exit category: %v", child.id, err)
	}
}

func (child *processChild) readResult(t *testing.T) (string, string) {
	t.Helper()
	if !child.scan.Scan() {
		t.Fatalf("helper %s missing fixed result", child.id)
	}
	fields := strings.Fields(child.scan.Text())
	if len(fields) != 2 || !validCandidateOpaqueID(fields[1]) {
		t.Fatalf("helper %s invalid result shape", child.id)
	}
	switch fields[0] {
	case "ready", "opened", "ok", "existing", "conflict", "notfound", "closed", "busy", "corrupt", "unavailable", "invalid", "unsupported", "unknown", "failed":
	default:
		t.Fatalf("helper %s invalid result category", child.id)
	}
	return fields[0], fields[1]
}

func TestProcessConcurrencyGroundwork(t *testing.T) {
	if testing.Short() {
		t.Skip("subprocess concurrency groundwork")
	}
	directory := t.TempDir()
	if err := os.Chmod(directory, 0o700); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(directory, "memory.db")

	initializers := []*processChild{
		startProcessChild(t, path, "initializer-a", "init", nil),
		startProcessChild(t, path, "initializer-b", "init", nil),
	}
	for _, child := range initializers {
		child.release(t)
	}
	for _, child := range initializers {
		child.awaitOpened(t)
	}
	for _, child := range initializers {
		child.releaseAction(t)
	}
	var databaseID string
	for index, child := range initializers {
		category, id := child.finish(t)
		if category != "ok" || index != 0 && id != databaseID {
			t.Fatalf("initializer %d = %s %s, first %s", index, category, id, databaseID)
		}
		databaseID = id
	}

	runChildren := func(specs []struct {
		id, action string
		values     map[string]string
	}) [][2]string {
		t.Helper()
		children := make([]*processChild, len(specs))
		// Open sequentially, then release actions simultaneously. Only virgin
		// initialization intentionally races Open; Phase 2's lifetime lock is
		// required before claiming arbitrary concurrent open/close support.
		for index, spec := range specs {
			children[index] = startProcessChild(t, path, spec.id, spec.action, spec.values)
			children[index].release(t)
			children[index].awaitOpened(t)
		}
		for _, child := range children {
			child.releaseAction(t)
		}
		results := make([][2]string, len(children))
		for index, child := range children {
			category, id := child.finish(t)
			results[index] = [2]string{category, id}
		}
		return results
	}
	values := func(entity, scope, key string) map[string]string {
		return map[string]string{"OTTO_SQLITE_ENTITY_ID": entity, "OTTO_SQLITE_SCOPE_ID": scope, "OTTO_SQLITE_KEY": key}
	}

	uniqueSpecs := make([]struct {
		id, action string
		values     map[string]string
	}, 4)
	for index := range uniqueSpecs {
		id := fmt.Sprintf("unique-%d", index)
		uniqueSpecs[index] = struct {
			id, action string
			values     map[string]string
		}{"unique-child-" + id, "create", values(id, "process-unique", "key-"+id)}
	}
	for _, result := range runChildren(uniqueSpecs) {
		if result[0] != "ok" {
			t.Fatalf("unique create = %v", result)
		}
	}

	createRace := runChildren([]struct {
		id, action string
		values     map[string]string
	}{
		{"create-race-a", "create", values("create-race-record-a", "process-create-race", "same-key")},
		{"create-race-b", "create", values("create-race-record-b", "process-create-race", "same-key")},
	})
	assertProcessCategories(t, createRace, "ok", "conflict")

	parent, err := Open(context.Background(), path, Options{Guard: memory.NewCompositeGuard(memory.DefaultGuard{}), NewID: memory.NewID})
	if err != nil {
		t.Fatal(err)
	}
	updateScope := memory.Scope{Namespace: "user", ID: "process-update-race"}
	if _, err := parent.Upsert(context.Background(), memory.UpsertRequest{Record: processRecord("update-race-record", updateScope, "update-key")}); err != nil {
		t.Fatal(err)
	}
	if err := parent.Close(); err != nil {
		t.Fatal(err)
	}
	updateRace := runChildren([]struct {
		id, action string
		values     map[string]string
	}{
		{"update-race-a", "update", values("update-race-record", updateScope.ID, "update-key")},
		{"update-race-b", "update", values("update-race-record", updateScope.ID, "update-key")},
	})
	assertProcessCategories(t, updateRace, "ok", "conflict")

	observationValuesA := values("observation-candidate-a", "process-observation", "")
	observationValuesA["OTTO_SQLITE_OBSERVATION_ID"] = "process-observation-id"
	observationValuesB := values("observation-candidate-b", "process-observation", "")
	observationValuesB["OTTO_SQLITE_OBSERVATION_ID"] = "process-observation-id"
	observationRace := runChildren([]struct {
		id, action string
		values     map[string]string
	}{
		{"observation-child-a", "observe", observationValuesA},
		{"observation-child-b", "observe", observationValuesB},
	})
	assertProcessCategories(t, observationRace, "ok", "existing")
	if observationRace[0][1] != observationRace[1][1] {
		t.Fatalf("observation receipt IDs differ: %v", observationRace)
	}

	parent, err = Open(context.Background(), path, Options{Guard: memory.NewCompositeGuard(memory.DefaultGuard{}), NewID: memory.NewID})
	if err != nil {
		t.Fatal(err)
	}
	reviewScope := memory.Scope{Namespace: "user", ID: "process-review"}
	candidate := processCandidate("process-review-candidate", reviewScope)
	if _, err := parent.Propose(context.Background(), memory.ProposalBatch{Candidates: []memory.Candidate{candidate}}); err != nil {
		t.Fatal(err)
	}
	tombstoneScope := memory.Scope{Namespace: "user", ID: "process-tombstone"}
	tombstoneRecord := processRecord("process-tombstone-record", tombstoneScope, "tombstone-key")
	created, err := parent.Upsert(context.Background(), memory.UpsertRequest{Record: tombstoneRecord})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := parent.Forget(context.Background(), memory.StoreForgetRequest{Ref: memory.RecordRef{Scope: tombstoneScope, ID: created.ID}, ExpectedRevision: 1, ForgottenAt: created.UpdatedAt.Add(time.Second)}); err != nil {
		t.Fatal(err)
	}
	if err := parent.Close(); err != nil {
		t.Fatal(err)
	}
	reviewRace := runChildren([]struct {
		id, action string
		values     map[string]string
	}{
		{"review-child-a", "review", values(candidate.ID, reviewScope.ID, "")},
		{"review-child-b", "review", values(candidate.ID, reviewScope.ID, "")},
	})
	assertProcessCategories(t, reviewRace, "ok", "conflict")

	stale := runChildren([]struct {
		id, action string
		values     map[string]string
	}{{"stale-child", "update", values(tombstoneRecord.ID, tombstoneScope.ID, tombstoneRecord.Key)}})
	if stale[0][0] != "conflict" {
		t.Fatalf("stale tombstone revival = %v", stale)
	}

	exitChild := startProcessChild(t, path, "postcommit-exit-child", "create-exit", values("postcommit-record", "postcommit-scope", "postcommit-key"))
	exitChild.release(t)
	exitChild.awaitOpened(t)
	exitChild.releaseAction(t)
	exitChild.finishWithoutResult(t)
	parent, err = Open(context.Background(), path, Options{Guard: memory.NewCompositeGuard(memory.DefaultGuard{}), NewID: memory.NewID})
	if err != nil {
		t.Fatal(err)
	}
	defer parent.Close()
	got, err := parent.Get(context.Background(), memory.RecordRef{Scope: memory.Scope{Namespace: "user", ID: "postcommit-scope"}, ID: "postcommit-record"})
	if err != nil || got.Revision != 1 {
		t.Fatalf("postcommit reconciliation category = %v revision=%d", err, got.Revision)
	}
	if _, err := parent.Upsert(context.Background(), memory.UpsertRequest{Record: processRecord("postcommit-record", got.Scope, "postcommit-key")}); !errors.Is(err, memory.ErrConflict) {
		t.Fatalf("postcommit duplicate retry = %v", err)
	}
}

func assertProcessCategories(t *testing.T, results [][2]string, first, second string) {
	t.Helper()
	counts := map[string]int{}
	for _, result := range results {
		counts[result[0]]++
	}
	if counts[first] != 1 || counts[second] != 1 {
		t.Fatalf("process categories = %v", results)
	}
}
