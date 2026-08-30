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
	"reflect"
	"runtime"
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
)

const processHelperEnvironment = "OTTO_SQLITE_PROCESS_HELPER"

func TestMain(m *testing.M) {
	if os.Getenv(processHelperEnvironment) == "1" {
		runProcessHelper()
		os.Exit(0)
	}
	os.Exit(m.Run())
}

// Kept as the documented helper selector; TestMain intercepts helper mode
// before the testing package can append status output to the stdout protocol.
func TestProcessHelper(*testing.T) {}

func runProcessHelper() {
	childID := os.Getenv("OTTO_SQLITE_CHILD_ID")
	input := bufio.NewReader(os.Stdin)
	fmt.Printf("armed %s\n", childID)
	if _, err := input.ReadString('\n'); err != nil {
		printProcessResult("failed", childID)
		return
	}
	fmt.Printf("ready %s\n", childID)
	if _, err := input.ReadString('\n'); err != nil {
		printProcessResult("failed", childID)
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
	if _, err := input.ReadString('\n'); err != nil {
		printProcessResult("failed", childID)
		return
	}
	fmt.Printf("ready %s\n", childID)
	if _, err := input.ReadString('\n'); err != nil {
		printProcessResult("failed", childID)
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
	if category, id := child.readResult(t); category != "armed" || id != childID {
		t.Fatal("helper arm state mismatch")
	}
	return child
}

func (child *processChild) send(t *testing.T, command string) {
	t.Helper()
	if _, err := io.WriteString(child.stdin, command+"\n"); err != nil {
		t.Fatal(err)
	}
}

func (child *processChild) arm(t *testing.T) {
	t.Helper()
	child.send(t, "arm")
	category, id := child.readResult(t)
	if category != "ready" || id != child.id {
		t.Fatal("helper ready barrier mismatch")
	}
}

func (child *processChild) goOpen(t *testing.T) {
	t.Helper()
	child.send(t, "go")
}

func (child *processChild) awaitOpened(t *testing.T) {
	t.Helper()
	category, id := child.readResult(t)
	if category != "opened" || id != child.id {
		t.Fatal("helper open barrier mismatch")
	}
}

func (child *processChild) armAction(t *testing.T) {
	t.Helper()
	child.arm(t)
}

func (child *processChild) releaseAction(t *testing.T) {
	t.Helper()
	child.send(t, "go")
	if err := child.stdin.Close(); err != nil {
		t.Fatal(err)
	}
}

func (child *processChild) finish(t *testing.T) (string, string) {
	t.Helper()
	defer child.cancel()
	category, id := child.readResult(t)
	if child.scan.Scan() {
		t.Fatalf("helper %s emitted trailing stdout", child.id)
	}
	if child.scan.Err() != nil {
		t.Fatalf("helper %s stdout read failed", child.id)
	}
	if err := child.cmd.Wait(); err != nil {
		t.Fatalf("helper %s exit category: %v", child.id, err)
	}
	return category, id
}

func (child *processChild) finishWithoutResult(t *testing.T) {
	t.Helper()
	defer child.cancel()
	if child.scan.Scan() {
		t.Fatalf("helper %s unexpectedly emitted stdout", child.id)
	}
	if err := child.scan.Err(); err != nil {
		t.Fatalf("helper %s stdout read failed", child.id)
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
	case "armed", "ready", "opened", "ok", "existing", "conflict", "notfound", "closed", "busy", "corrupt", "unavailable", "invalid", "unsupported", "unknown", "failed":
	default:
		t.Fatalf("helper %s invalid result category", child.id)
	}
	return fields[0], fields[1]
}

func TestProcessConcurrencyGroundwork(t *testing.T) {
	if runtime.GOOS != "darwin" && runtime.GOOS != "linux" {
		t.Skip("secure SQLite paths target darwin/linux")
	}
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
		child.arm(t)
	}
	for _, child := range initializers {
		child.goOpen(t)
	}
	for _, child := range initializers {
		child.awaitOpened(t)
	}
	for _, child := range initializers {
		child.armAction(t)
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
			children[index].arm(t)
			children[index].goOpen(t)
			children[index].awaitOpened(t)
		}
		for _, child := range children {
			child.armAction(t)
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
			t.Fatalf("unique create category = %v", result)
		}
	}
	parentOptions := Options{Guard: memory.NewCompositeGuard(memory.DefaultGuard{}), NewID: memory.NewID}
	openParent := func() *Store {
		t.Helper()
		store, err := Open(context.Background(), path, parentOptions)
		if err != nil {
			t.Fatal(err)
		}
		return store
	}
	parent := openParent()
	identity, err := parent.Identity(context.Background())
	if err != nil || identity.DatabaseID != databaseID || identity.SchemaVersion != schemaVersion || identity.Generation != 4 {
		t.Fatalf("identity after four creates = %#v, %v", identity, err)
	}
	uniqueScope := memory.Scope{Namespace: "user", ID: "process-unique"}
	for index := 0; index < 4; index++ {
		id := fmt.Sprintf("unique-%d", index)
		got, err := parent.Get(context.Background(), memory.RecordRef{Scope: uniqueScope, ID: id})
		want := processRecord(id, uniqueScope, "key-"+id)
		want.Revision = 1
		want.Source.MessageIDs = []string{}
		if err != nil || !reflect.DeepEqual(got, want) {
			t.Fatalf("durable unique create %d = %#v, want %#v, %v", index, got, want, err)
		}
	}
	beforeCreate := identity.Generation
	if err := parent.Close(); err != nil {
		t.Fatal(err)
	}

	createRace := runChildren([]struct {
		id, action string
		values     map[string]string
	}{
		{"create-race-a", "create", values("create-race-record-a", "process-create-race", "same-key")},
		{"create-race-b", "create", values("create-race-record-b", "process-create-race", "same-key")},
	})
	assertProcessCategories(t, createRace, "ok", "conflict")
	createWinner, createLoser := createRace[0][1], createRace[1][1]
	if createRace[1][0] == "ok" {
		createWinner, createLoser = createRace[1][1], createRace[0][1]
	}
	parent = openParent()
	identity, err = parent.Identity(context.Background())
	if err != nil || identity.Generation != beforeCreate+1 {
		t.Fatalf("create race generation = %d, want %d, %v", identity.Generation, beforeCreate+1, err)
	}
	createScope := memory.Scope{Namespace: "user", ID: "process-create-race"}
	winnerRecord, err := parent.GetByKey(context.Background(), memory.RecordKey{Scope: createScope, Kind: "note", Key: "same-key"})
	if err != nil || winnerRecord.ID != createWinner || winnerRecord.Revision != 1 || winnerRecord.Text != "fixed process value" {
		t.Fatalf("durable create winner = %#v, %v", winnerRecord, err)
	}
	if _, err := parent.Get(context.Background(), memory.RecordRef{Scope: createScope, ID: createLoser}); !errors.Is(err, memory.ErrNotFound) {
		t.Fatalf("durable create loser = %v", err)
	}

	updateScope := memory.Scope{Namespace: "user", ID: "process-update-race"}
	if _, err := parent.Upsert(context.Background(), memory.UpsertRequest{Record: processRecord("update-race-record", updateScope, "update-key")}); err != nil {
		t.Fatal(err)
	}
	beforeUpdate, err := parent.Identity(context.Background())
	if err != nil {
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
	parent = openParent()
	identity, err = parent.Identity(context.Background())
	updated, updateErr := parent.Get(context.Background(), memory.RecordRef{Scope: updateScope, ID: "update-race-record"})
	if err != nil || updateErr != nil || identity.Generation != beforeUpdate.Generation+1 || updated.Revision != 2 || updated.Text != "fixed updated value" || !updated.UpdatedAt.Equal(processTime().Add(time.Second)) {
		t.Fatalf("durable update state=%#v identity=%#v errors=%v/%v", updated, identity, err, updateErr)
	}
	beforeObservation := identity.Generation
	if err := parent.Close(); err != nil {
		t.Fatal(err)
	}

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
	observationWinner := observationRace[0][1]
	observationLoser := "observation-candidate-b"
	if observationWinner == observationLoser {
		observationLoser = "observation-candidate-a"
	}
	parent = openParent()
	identity, err = parent.Identity(context.Background())
	receipt, receiptErr := parent.GetObservationReceipt(context.Background(), "process-observation-id")
	observationScope := memory.Scope{Namespace: "user", ID: "process-observation"}
	winnerCandidate, candidateErr := parent.GetCandidate(context.Background(), memory.CandidateRef{Scope: observationScope, ID: observationWinner})
	if err != nil || receiptErr != nil || candidateErr != nil || identity.Generation != beforeObservation+1 || !receipt.Existing || !reflect.DeepEqual(receipt.CandidateIDs, []string{observationWinner}) || winnerCandidate.Proposed.Source.ObservationID != receipt.ObservationID {
		t.Fatalf("durable observation identity=%#v receipt=%#v candidate=%#v errors=%v/%v/%v", identity, receipt, winnerCandidate, err, receiptErr, candidateErr)
	}
	if _, err := parent.GetCandidate(context.Background(), memory.CandidateRef{Scope: observationScope, ID: observationLoser}); !errors.Is(err, memory.ErrNotFound) {
		t.Fatalf("observation loser persisted = %v", err)
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
	forgottenAt := created.UpdatedAt.Add(time.Second)
	if _, err := parent.Forget(context.Background(), memory.StoreForgetRequest{Ref: memory.RecordRef{Scope: tombstoneScope, ID: created.ID}, ExpectedRevision: 1, ForgottenAt: forgottenAt}); err != nil {
		t.Fatal(err)
	}
	beforeReview, err := parent.Identity(context.Background())
	if err != nil {
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
	parent = openParent()
	identity, err = parent.Identity(context.Background())
	decided, reviewErr := parent.GetCandidate(context.Background(), memory.CandidateRef{Scope: reviewScope, ID: candidate.ID})
	if err != nil || reviewErr != nil || identity.Generation != beforeReview.Generation+1 || decided.State != memory.CandidateRejected || decided.DecisionSource != memory.OriginHuman || decided.DecidedAt == nil || !decided.DecidedAt.Equal(processTime().Add(4*time.Second)) {
		t.Fatalf("durable review identity=%#v candidate=%#v errors=%v/%v", identity, decided, err, reviewErr)
	}
	if err := parent.Close(); err != nil {
		t.Fatal(err)
	}

	stale := runChildren([]struct {
		id, action string
		values     map[string]string
	}{{"stale-child", "update", values(tombstoneRecord.ID, tombstoneScope.ID, tombstoneRecord.Key)}})
	if stale[0][0] != "conflict" {
		t.Fatalf("stale tombstone revival = %v", stale)
	}
	parent = openParent()
	tombstone, tombstoneErr := parent.GetTombstone(context.Background(), memory.RecordRef{Scope: tombstoneScope, ID: tombstoneRecord.ID})
	identity, err = parent.Identity(context.Background())
	if tombstoneErr != nil || err != nil || tombstone.Revision != 2 || !tombstone.ForgottenAt.Equal(forgottenAt) || identity.Generation != beforeReview.Generation+1 {
		t.Fatalf("tombstone after stale attempt=%#v identity=%#v errors=%v/%v", tombstone, identity, tombstoneErr, err)
	}
	if _, err := parent.Get(context.Background(), memory.RecordRef{Scope: tombstoneScope, ID: tombstoneRecord.ID}); !errors.Is(err, memory.ErrNotFound) {
		t.Fatalf("stale update revived active row: %v", err)
	}
	if err := parent.Close(); err != nil {
		t.Fatal(err)
	}

	exitChild := startProcessChild(t, path, "postcommit-exit-child", "create-exit", values("postcommit-record", "postcommit-scope", "postcommit-key"))
	exitChild.arm(t)
	exitChild.goOpen(t)
	exitChild.awaitOpened(t)
	exitChild.armAction(t)
	exitChild.releaseAction(t)
	exitChild.finishWithoutResult(t)
	parent = openParent()
	postcommitScope := memory.Scope{Namespace: "user", ID: "postcommit-scope"}
	gotBefore, err := parent.Get(context.Background(), memory.RecordRef{Scope: postcommitScope, ID: "postcommit-record"})
	identityBeforeRetry, identityErr := parent.Identity(context.Background())
	if err != nil || identityErr != nil || gotBefore.Revision != 1 || gotBefore.Text != "fixed process value" {
		t.Fatalf("postcommit before retry=%#v identity=%#v errors=%v/%v", gotBefore, identityBeforeRetry, err, identityErr)
	}
	if _, err := parent.Upsert(context.Background(), memory.UpsertRequest{Record: processRecord("postcommit-record", gotBefore.Scope, "postcommit-key")}); !errors.Is(err, memory.ErrConflict) {
		t.Fatalf("postcommit duplicate retry = %v", err)
	}
	gotAfter, err := parent.Get(context.Background(), memory.RecordRef{Scope: postcommitScope, ID: "postcommit-record"})
	identityAfterRetry, identityErr := parent.Identity(context.Background())
	if err != nil || identityErr != nil || !reflect.DeepEqual(gotAfter, gotBefore) || identityAfterRetry.Generation != identityBeforeRetry.Generation {
		t.Fatalf("postcommit after retry=%#v identity=%#v errors=%v/%v", gotAfter, identityAfterRetry, err, identityErr)
	}
	if err := parent.Close(); err != nil {
		t.Fatal(err)
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
