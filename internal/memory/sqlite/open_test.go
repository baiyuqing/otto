package sqlite

import (
	"context"
	"crypto/sha256"
	"database/sql"
	"errors"
	"fmt"
	"io"
	"math"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
	"golang.org/x/sys/unix"
)

const (
	testDatabaseID = "0123456789abcdef0123456789abcdef"
	testUserID     = "fedcba9876543210fedcba9876543210"
	testForbidden  = "deaddeaddeaddeaddeaddeaddeaddead"
)

func testGuard(t *testing.T, forbidden ...string) *memory.CompositeGuard {
	t.Helper()
	exact, err := memory.NewExactGuard(forbidden)
	if err != nil {
		t.Fatal(err)
	}
	return memory.NewCompositeGuard(memory.DefaultGuard{}, exact)
}

func testGuardWith(t *testing.T, member memory.ContentGuard, forbidden ...string) *memory.CompositeGuard {
	t.Helper()
	exact, err := memory.NewExactGuard(forbidden)
	if err != nil {
		t.Fatal(err)
	}
	return memory.NewCompositeGuard(memory.DefaultGuard{}, exact, member)
}

func testOptions(t *testing.T) Options {
	t.Helper()
	ids := []string{testDatabaseID, testUserID}
	var index atomic.Int64
	return Options{
		Guard: testGuard(t),
		NewID: func() (string, error) {
			i := index.Add(1) - 1
			if i >= int64(len(ids)) {
				return "", errors.New("unexpected ID request")
			}
			return ids[i], nil
		},
	}
}

func installTestHooks(t *testing.T, hooks testHooks) {
	t.Helper()
	t.Cleanup(setTestHooks(hooks))
}

func openTestStore(t *testing.T, path string, mutate ...func(*Options)) *Store {
	t.Helper()
	if _, err := os.Stat(filepath.Dir(path)); err == nil {
		if err := os.Chmod(filepath.Dir(path), 0o700); err != nil {
			t.Fatal(err)
		}
	}
	options := testOptions(t)
	for _, fn := range mutate {
		fn(&options)
	}
	store, err := Open(context.Background(), path, options)
	if err != nil {
		t.Fatalf("Open() error = %v", err)
	}
	t.Cleanup(func() {
		if err := store.Close(); err != nil {
			t.Errorf("Close() error = %v", err)
		}
	})
	return store
}

func assertMode(t *testing.T, path string, want os.FileMode) {
	t.Helper()
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := info.Mode().Perm(); got != want {
		t.Fatalf("%s mode = %04o, want %04o", filepath.Base(path), got, want)
	}
}

func assertNoPrivateDirectoryTemps(t *testing.T, root string) {
	t.Helper()
	err := filepath.WalkDir(root, func(path string, entry os.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if strings.HasPrefix(entry.Name(), ".otto-memory-directory-") {
			t.Errorf("temporary directory leaked: %s", filepath.Base(path))
		}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
}

func assertSafeError(t *testing.T, err error, category error, secrets ...string) {
	t.Helper()
	if !errors.Is(err, category) {
		t.Fatalf("error = %v, want %v", err, category)
	}
	for _, secret := range secrets {
		if secret != "" && strings.Contains(err.Error(), secret) {
			t.Fatalf("safe error contains %q: %v", secret, err)
		}
	}
}

func queryInt(t *testing.T, conn *sql.Conn, query string) int64 {
	t.Helper()
	var value int64
	if err := conn.QueryRowContext(context.Background(), query).Scan(&value); err != nil {
		t.Fatalf("%s: %v", query, err)
	}
	return value
}

func queryString(t *testing.T, conn *sql.Conn, query string, args ...any) string {
	t.Helper()
	var value string
	if err := conn.QueryRowContext(context.Background(), query, args...).Scan(&value); err != nil {
		t.Fatalf("%s: %v", query, err)
	}
	return value
}

func borrowTestConn(t *testing.T, store *Store) *sql.Conn {
	t.Helper()
	select {
	case conn := <-store.connections:
		t.Cleanup(func() { store.connections <- conn })
		return conn
	case <-time.After(time.Second):
		t.Fatal("timed out borrowing retained connection")
		return nil
	}
}

func TestOpenCreatesPrivateFilesAndFourConfiguredConnections(t *testing.T) {
	parent := filepath.Join(t.TempDir(), "new", "dedicated")
	path := filepath.Join(parent, "memory.db")
	store := openTestStore(t, path)

	assertMode(t, parent, 0o700)
	assertMode(t, path, 0o600)
	if got := cap(store.connections); got != retainedConnectionCount {
		t.Fatalf("connection capacity = %d, want %d", got, retainedConnectionCount)
	}
	if got := len(store.connections); got != retainedConnectionCount {
		t.Fatalf("retained connections = %d, want %d", got, retainedConnectionCount)
	}

	borrowed := make([]*sql.Conn, 0, retainedConnectionCount)
	for range retainedConnectionCount {
		select {
		case conn := <-store.connections:
			borrowed = append(borrowed, conn)
		default:
			t.Fatal("fewer than four retained connections")
		}
	}
	for i, conn := range borrowed {
		if got := queryInt(t, conn, "PRAGMA foreign_keys"); got != 1 {
			t.Errorf("connection %d foreign_keys = %d", i, got)
		}
		if got := queryInt(t, conn, "PRAGMA synchronous"); got != 2 {
			t.Errorf("connection %d synchronous = %d, want FULL(2)", i, got)
		}
		if got := queryInt(t, conn, "PRAGMA busy_timeout"); got != 5000 {
			t.Errorf("connection %d busy_timeout = %d", i, got)
		}
		if got := queryInt(t, conn, "PRAGMA trusted_schema"); got != 0 {
			t.Errorf("connection %d trusted_schema = %d", i, got)
		}
		if _, err := conn.ExecContext(context.Background(), `SELECT "DQS must not become a string"`); err == nil {
			t.Errorf("connection %d accepted DQS fallback", i)
		}
		if _, err := conn.ExecContext(context.Background(), "PRAGMA writable_schema=ON"); err != nil {
			t.Errorf("connection %d writable_schema probe: %v", i, err)
		}
		if got := queryInt(t, conn, "PRAGMA writable_schema"); got != 0 {
			t.Errorf("connection %d writable_schema = %d, defensive mode is off", i, got)
		}
		for limit, want := range sqliteLimitCeilings {
			got, err := connectionLimit(conn, limit, -1)
			if err != nil {
				t.Errorf("connection %d limit %d: %v", i, limit, err)
			} else if got != want {
				t.Errorf("connection %d limit %d = %d, want %d", i, limit, got, want)
			}
		}
	}
	if got := queryString(t, borrowed[0], "PRAGMA journal_mode"); strings.ToLower(got) != "wal" {
		t.Fatalf("journal_mode = %q", got)
	}
	if got := queryString(t, borrowed[0], "PRAGMA quick_check"); got != "ok" {
		t.Fatalf("quick_check = %q", got)
	}
	for _, conn := range borrowed {
		store.connections <- conn
	}

	if err := store.withWrite(context.Background(), memory.CommitSchema, nil, func(ctx context.Context, conn *sql.Conn) error {
		_, err := conn.ExecContext(ctx, `INSERT INTO memory_records(
			id,scope_namespace,scope_id,kind,semantic_key,text_value,labels_json,metadata_json,source_json,
			confidence,revision,created_at,updated_at,expires_at,state,forgotten_at
		) VALUES('sidecar','user','u','note','','hello','[]','{}','{}',1,1,
			'2026-01-01T00:00:00.000000000Z','2026-01-01T00:00:00.000000000Z',NULL,'active',NULL)`)
		return err
	}); err != nil {
		t.Fatalf("write: %v", err)
	}
	for _, suffix := range []string{"-wal", "-shm"} {
		if info, err := os.Lstat(path + suffix); err == nil {
			if info.Mode().Perm()&0o077 != 0 {
				t.Errorf("%s mode = %04o", suffix, info.Mode().Perm())
			}
		} else if !errors.Is(err, os.ErrNotExist) {
			t.Error(err)
		}
	}
}

func TestOpenRejectsOwnerBitStrippingUmaskWithoutBroadeningConcurrentCreation(t *testing.T) {
	if runtime.GOOS != "darwin" && runtime.GOOS != "linux" {
		t.Skip("secure path implementation is Unix-only")
	}
	const helperEnvironment = "OTTO_TEST_OWNER_BIT_STRIPPING_UMASK"
	if os.Getenv(helperEnvironment) == "" {
		command := exec.Command(os.Args[0], "-test.run=^TestOpenRejectsOwnerBitStrippingUmaskWithoutBroadeningConcurrentCreation$")
		command.Env = append(os.Environ(), helperEnvironment+"=1")
		if output, err := command.CombinedOutput(); err != nil {
			t.Fatalf("isolated umask regression failed: %v\n%s", err, output)
		}
		return
	}

	root := t.TempDir()
	old := syscall.Umask(0o777)
	defer syscall.Umask(old)

	unrelated := filepath.Join(root, "unrelated")
	var createErr error
	var createMode os.FileMode
	installTestHooks(t, testHooks{mkdirat: func(dirfd int, name string, mode uint32) error {
		if !connectionProofMu.TryLock() {
			t.Fatal("Mkdirat test seam ran under the connection proof lock")
		}
		connectionProofMu.Unlock()
		created := make(chan struct{})
		go func() {
			createErr = os.WriteFile(unrelated, nil, 0o666)
			if createErr == nil {
				var info os.FileInfo
				info, createErr = os.Stat(unrelated)
				if createErr == nil {
					createMode = info.Mode().Perm()
				}
			}
			close(created)
		}()
		<-created
		return unix.Mkdirat(dirfd, name, mode)
	}})

	parent := filepath.Join(root, "new", "private")
	path := filepath.Join(parent, "memory.db")
	_, err := Open(context.Background(), path, testOptions(t))
	assertSafeError(t, err, memory.ErrUnavailable, path)
	if createErr != nil {
		t.Fatalf("create unrelated file: %v", createErr)
	}
	if createMode != 0 {
		t.Fatalf("unrelated file mode = %04o, want 0000 from ambient umask", createMode)
	}
	if _, statErr := os.Lstat(parent); !errors.Is(statErr, os.ErrNotExist) {
		t.Fatalf("target installed under owner-bit-stripping umask: %v", statErr)
	}
	assertNoPrivateDirectoryTemps(t, root)
}

func TestOpenDirectoryInstallUsesNoReplaceAndCleansTemporaryDirectories(t *testing.T) {
	if runtime.GOOS != "darwin" && runtime.GOOS != "linux" {
		t.Skip("secure path implementation is Unix-only")
	}

	t.Run("concurrent winner remains unmodified", func(t *testing.T) {
		root := t.TempDir()
		parent := filepath.Join(root, "winner")
		path := filepath.Join(parent, "memory.db")
		var installed atomic.Bool
		installTestHooks(t, testHooks{beforeDirectoryInstall: func(name string) {
			if name != filepath.Base(parent) || !installed.CompareAndSwap(false, true) {
				return
			}
			if err := os.Mkdir(parent, 0o750); err != nil {
				panic(err)
			}
			if err := os.Chmod(parent, 0o750); err != nil {
				panic(err)
			}
		}})

		_, err := Open(context.Background(), path, testOptions(t))
		assertSafeError(t, err, memory.ErrInvalidRequest, path)
		if !installed.Load() {
			t.Fatal("directory install hook was not reached")
		}
		assertMode(t, parent, 0o750)
		assertNoPrivateDirectoryTemps(t, root)
	})

	t.Run("cancellation removes prepared directory", func(t *testing.T) {
		root := t.TempDir()
		parent := filepath.Join(root, "canceled")
		path := filepath.Join(parent, "memory.db")
		ctx, cancel := context.WithCancel(context.Background())
		var canceled atomic.Bool
		installTestHooks(t, testHooks{beforeDirectoryInstall: func(name string) {
			if name == filepath.Base(parent) && canceled.CompareAndSwap(false, true) {
				cancel()
			}
		}})

		_, err := Open(ctx, path, testOptions(t))
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("Open = %v, want context cancellation", err)
		}
		if !canceled.Load() {
			t.Fatal("directory install hook was not reached")
		}
		if _, statErr := os.Lstat(parent); !errors.Is(statErr, os.ErrNotExist) {
			t.Fatalf("target installed after cancellation: %v", statErr)
		}
		assertNoPrivateDirectoryTemps(t, root)
	})
}

func TestIdentityPersistsAndIsGuarded(t *testing.T) {
	path := filepath.Join(t.TempDir(), "identity", "memory.db")
	store := openTestStore(t, path)
	identity, err := store.Identity(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if identity.DatabaseID != testDatabaseID || identity.UserScope.Namespace != "user" || identity.UserScope.ID != testUserID {
		t.Fatalf("identity = %#v", identity)
	}
	if identity.DatabaseID == identity.UserScope.ID || len(identity.DatabaseID) != 32 || len(identity.UserScope.ID) != 32 {
		t.Fatalf("IDs are not distinct 32-hex values: %#v", identity)
	}
	if identity.SchemaVersion != 1 || identity.Generation != 0 {
		t.Fatalf("schema/generation = %d/%d", identity.SchemaVersion, identity.Generation)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := store.Identity(context.Background()); !errors.Is(err, memory.ErrClosed) {
		t.Fatalf("Identity after close = %v", err)
	}
	if err := store.Close(); err != nil {
		t.Fatalf("second Close = %v", err)
	}

	reopened, err := Open(context.Background(), path, Options{Guard: testGuard(t), NewID: func() (string, error) {
		return "", errors.New("NewID must not run for initialized schema")
	}})
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()
	got, err := reopened.Identity(context.Background())
	if err != nil || got != identity {
		t.Fatalf("reopened identity = %#v, %v; want %#v", got, err, identity)
	}

	if err := reopened.Close(); err != nil {
		t.Fatal(err)
	}
	raw := openRaw(t, path)
	if _, err := raw.Exec(`UPDATE memory_meta SET value=? WHERE key='database_id'`, testForbidden); err != nil {
		t.Fatal(err)
	}
	if err := raw.Close(); err != nil {
		t.Fatal(err)
	}
	_, err = Open(context.Background(), path, Options{Guard: testGuard(t, testForbidden), NewID: memory.NewID})
	assertSafeError(t, err, memory.ErrSensitiveMemory, path, testForbidden)
}

func TestOpenCancellationAndCallbacksOutsideLocks(t *testing.T) {
	t.Run("already canceled creates nothing", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "canceled", "memory.db")
		ctx, cancel := context.WithCancel(context.Background())
		cancel()
		_, err := Open(ctx, path, testOptions(t))
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("Open = %v", err)
		}
		if _, statErr := os.Stat(path); !errors.Is(statErr, os.ErrNotExist) {
			t.Fatalf("database exists after canceled Open: %v", statErr)
		}
	})

	t.Run("canceled after driver open cleans up", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "post-open", "memory.db")
		ctx, cancel := context.WithCancel(context.Background())
		installTestHooks(t, testHooks{path: func(event pathEvent) {
			if event == pathAfterWriteDriverOpen {
				cancel()
			}
		}})
		_, err := Open(ctx, path, testOptions(t))
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("Open = %v", err)
		}
		if got := processFDCountForPath(t, path); got != 0 {
			t.Fatalf("database inode descriptor count after canceled Open = %d, want 0", got)
		}
	})

	t.Run("blocked NewID does not hold package initialization lock", func(t *testing.T) {
		root := t.TempDir()
		started := make(chan struct{})
		release := make(chan struct{})
		options := testOptions(t)
		base := options.NewID
		var once sync.Once
		options.NewID = func() (string, error) {
			once.Do(func() { close(started); <-release })
			return base()
		}
		firstDone := make(chan error, 1)
		firstPath := filepath.Join(root, "first", "memory.db")
		go func() {
			store, err := Open(context.Background(), firstPath, options)
			if store != nil {
				_ = store.Close()
			}
			firstDone <- err
		}()
		<-started

		secondDone := make(chan error, 1)
		go func() {
			store, err := Open(context.Background(), filepath.Join(root, "second", "memory.db"), testOptions(t))
			if store != nil {
				_ = store.Close()
			}
			secondDone <- err
		}()
		select {
		case err := <-secondDone:
			if err != nil {
				t.Fatalf("independent Open: %v", err)
			}
		case <-time.After(3 * time.Second):
			t.Fatal("NewID callback retained an initialization/proof lock")
		}
		close(release)
		if err := <-firstDone; err != nil {
			t.Fatal(err)
		}
	})

	t.Run("setup failure closes every physically opened connection", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "setup-cleanup", "memory.db")
		initialized := openTestStore(t, path)
		if err := initialized.Close(); err != nil {
			t.Fatal(err)
		}
		ctx, cancel := context.WithCancel(context.Background())
		var opened atomic.Int64
		installTestHooks(t, testHooks{path: func(event pathEvent) {
			switch event {
			case pathAfterWriteDriverOpen, pathAfterRetainedConnection2DriverOpen,
				pathAfterRetainedConnection3DriverOpen, pathAfterRetainedConnection4DriverOpen:
				opened.Add(1)
			}
			if event == pathAfterRetainedConnection3DriverOpen {
				cancel()
			}
		}})
		_, err := Open(ctx, path, testOptions(t))
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("Open = %v", err)
		}
		if got := opened.Load(); got != 3 {
			t.Fatalf("physical setup opens = %d, want 3", got)
		}
		if got := processFDCountForPath(t, path); got != 0 {
			t.Fatalf("database inode descriptors after setup failure = %d", got)
		}
	})
}

type callbackGuard struct {
	started     chan struct{}
	release     chan struct{}
	blockOnCall int64
	active      atomic.Bool
	reentered   atomic.Bool
	calls       atomic.Int64
	once        sync.Once
}

func (guard *callbackGuard) Check(ctx context.Context, _ memory.GuardInput) error {
	if !guard.active.CompareAndSwap(false, true) {
		guard.reentered.Store(true)
		return memory.ErrUnavailable
	}
	defer guard.active.Store(false)
	call := guard.calls.Add(1)
	blockOnCall := guard.blockOnCall
	if blockOnCall == 0 {
		blockOnCall = 1
	}
	if call == blockOnCall {
		guard.once.Do(func() { close(guard.started) })
		select {
		case <-ctx.Done():
		case <-guard.release:
		}
	}
	return ctx.Err()
}

func TestOpenGuardCallbackIsUnlockedAndNotReentered(t *testing.T) {
	root := t.TempDir()
	guard := &callbackGuard{started: make(chan struct{}), release: make(chan struct{}), blockOnCall: 1}
	options := testOptions(t)
	options.Guard = testGuardWith(t, guard)
	firstDone := make(chan error, 1)
	go func() {
		store, err := Open(context.Background(), filepath.Join(root, "guarded", "memory.db"), options)
		if store != nil {
			_ = store.Close()
		}
		firstDone <- err
	}()
	<-guard.started

	secondDone := make(chan error, 1)
	go func() {
		store, err := Open(context.Background(), filepath.Join(root, "independent", "memory.db"), testOptions(t))
		if store != nil {
			_ = store.Close()
		}
		secondDone <- err
	}()
	select {
	case err := <-secondDone:
		if err != nil {
			t.Fatalf("independent Open while Guard blocked: %v", err)
		}
	case <-time.After(3 * time.Second):
		t.Fatal("Guard callback held a SQL/proof/initialization lock")
	}
	close(guard.release)
	if err := <-firstDone; err != nil {
		t.Fatal(err)
	}
	if guard.reentered.Load() {
		t.Fatal("Guard callback was reentered")
	}
	calls := guard.calls.Load()
	if calls != 2 {
		t.Fatalf("Guard calls = %d, want prospective and published identity checks", calls)
	}
	if guard.calls.Load() != calls {
		t.Fatal("Guard callback was retained after Open")
	}
}

func TestOpenPublishedGuardRunsAfterStoreOwnsEveryConnection(t *testing.T) {
	root := t.TempDir()
	guard := &callbackGuard{
		started: make(chan struct{}), release: make(chan struct{}), blockOnCall: 2,
	}
	options := testOptions(t)
	options.Guard = testGuardWith(t, guard)
	var ready atomic.Pointer[Store]
	installTestHooks(t, testHooks{storeReady: func(store *Store) { ready.Store(store) }})
	done := make(chan error, 1)
	go func() {
		store, err := Open(context.Background(), filepath.Join(root, "guarded", "memory.db"), options)
		if store != nil {
			_ = store.Close()
		}
		done <- err
	}()
	<-guard.started

	store := ready.Load()
	if store == nil {
		t.Fatal("Store was not assembled before published identity Guard")
	}
	if got := len(store.connections); got != retainedConnectionCount {
		t.Fatalf("published Guard borrowed SQL connection: available = %d", got)
	}
	if !store.lifecycleMu.TryLock() {
		t.Fatal("published Guard held lifecycle lock")
	}
	store.lifecycleMu.Unlock()
	select {
	case <-store.writeGate:
		store.writeGate <- struct{}{}
	default:
		t.Fatal("published Guard held write gate")
	}
	if !connectionProofMu.TryLock() {
		t.Fatal("published Guard held connection proof lock")
	}
	connectionProofMu.Unlock()
	select {
	case <-initializationGate:
		initializationGate <- struct{}{}
	default:
		t.Fatal("published Guard held initialization gate")
	}

	independentDone := make(chan error, 1)
	go func() {
		other, err := Open(context.Background(), filepath.Join(root, "other", "memory.db"), testOptions(t))
		if other != nil {
			_ = other.Close()
		}
		independentDone <- err
	}()
	select {
	case err := <-independentDone:
		if err != nil {
			t.Fatalf("independent Open: %v", err)
		}
	case <-time.After(3 * time.Second):
		t.Fatal("published Guard blocked SQL/proof/lifecycle progress")
	}
	close(guard.release)
	if err := <-done; err != nil {
		t.Fatal(err)
	}
	if guard.reentered.Load() {
		t.Fatal("published Guard was reentered")
	}
}

func TestOpenCanonicalPathEncodingAndCrashWALRecovery(t *testing.T) {
	root := t.TempDir()
	realParent := filepath.Join(root, "physical parent")
	if err := os.Mkdir(realParent, 0o700); err != nil {
		t.Fatal(err)
	}
	linkParent := filepath.Join(root, "linked parent")
	if err := os.Symlink(realParent, linkParent); err != nil {
		t.Fatal(err)
	}
	name := "memory ?#% ü.sqlite"
	path := filepath.Join(linkParent, name)
	store := openTestStore(t, path)
	physical := filepath.Join(realParent, name)
	if _, err := os.Stat(physical); err != nil {
		t.Fatalf("encoded path did not open intended file: %v", err)
	}
	if _, err := os.Lstat(path); err != nil {
		t.Fatal(err)
	}

	if err := store.withWrite(context.Background(), memory.CommitSchema, nil, func(ctx context.Context, conn *sql.Conn) error {
		_, err := conn.ExecContext(ctx, `INSERT INTO memory_records(
			id,scope_namespace,scope_id,kind,semantic_key,text_value,labels_json,metadata_json,source_json,
			confidence,revision,created_at,updated_at,expires_at,state,forgotten_at
		) VALUES('crash-row','user','u','note','k','committed','[]','{}','{"origin":"human","session_id":"","message_ids":[],"observation_id":"","decision_at":null,"decision_source":""}',1,1,
			'2026-01-01T00:00:00.000000000Z','2026-01-01T00:00:00.000000000Z',NULL,'active',NULL)`)
		if err != nil {
			return err
		}
		_, err = conn.ExecContext(ctx, `INSERT INTO memory_records_fts(record_id,text_value,kind,semantic_key,labels) VALUES('crash-row','committed','note','k','')`)
		return err
	}); err != nil {
		t.Fatal(err)
	}
	crashDir := filepath.Join(root, "crash-copy")
	if err := os.Mkdir(crashDir, 0o700); err != nil {
		t.Fatal(err)
	}
	crashPath := filepath.Join(crashDir, "copy.db")
	copyFile(t, physical, crashPath, 0o600)
	copyFile(t, physical+"-wal", crashPath+"-wal", 0o600)

	crash, err := Open(context.Background(), crashPath, Options{Guard: testGuard(t), NewID: memory.NewID})
	if err != nil {
		t.Fatalf("open crash pair: %v", err)
	}
	defer crash.Close()
	conn := borrowTestConn(t, crash)
	if got := queryString(t, conn, `SELECT text_value FROM memory_records WHERE id='crash-row'`); got != "committed" {
		t.Fatalf("recovered value = %q", got)
	}
}

func copyFile(t *testing.T, source, target string, mode os.FileMode) {
	t.Helper()
	in, err := os.Open(source)
	if err != nil {
		t.Fatal(err)
	}
	defer in.Close()
	out, err := os.OpenFile(target, os.O_WRONLY|os.O_CREATE|os.O_EXCL, mode)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := io.Copy(out, in); err != nil {
		out.Close()
		t.Fatal(err)
	}
	if err := out.Close(); err != nil {
		t.Fatal(err)
	}
}

func TestOpenRejectsUnsafePathsAndNeverChmodsExistingEntries(t *testing.T) {
	if runtime.GOOS != "darwin" && runtime.GOOS != "linux" {
		t.Skip("secure path implementation is Unix-only")
	}
	t.Run("existing parent permissions", func(t *testing.T) {
		parent := filepath.Join(t.TempDir(), "unsafe-parent-marker")
		if err := os.Mkdir(parent, 0o750); err != nil {
			t.Fatal(err)
		}
		if err := os.Chmod(parent, 0o750); err != nil {
			t.Fatal(err)
		}
		path := filepath.Join(parent, "memory.db")
		_, err := Open(context.Background(), path, testOptions(t))
		assertSafeError(t, err, memory.ErrInvalidRequest, path, "unsafe-parent-marker")
		assertMode(t, parent, 0o750)
	})

	for _, tc := range []struct {
		name  string
		setup func(t *testing.T, path string)
	}{
		{"database symlink", func(t *testing.T, path string) {
			target := filepath.Join(t.TempDir(), "target")
			if err := os.WriteFile(target, nil, 0o600); err != nil {
				t.Fatal(err)
			}
			if err := os.Symlink(target, path); err != nil {
				t.Fatal(err)
			}
		}},
		{"database directory", func(t *testing.T, path string) {
			if err := os.Mkdir(path, 0o700); err != nil {
				t.Fatal(err)
			}
		}},
		{"database public mode", func(t *testing.T, path string) {
			if err := os.WriteFile(path, nil, 0o640); err != nil {
				t.Fatal(err)
			}
			if err := os.Chmod(path, 0o640); err != nil {
				t.Fatal(err)
			}
		}},
		{"database preexisting owner mode", func(t *testing.T, path string) {
			if err := os.WriteFile(path, nil, 0); err != nil {
				t.Fatal(err)
			}
		}},
		{"WAL symlink", func(t *testing.T, path string) {
			writeEmptyPrivateFile(t, path)
			if err := os.Symlink(path, path+"-wal"); err != nil {
				t.Fatal(err)
			}
		}},
		{"WAL directory", func(t *testing.T, path string) {
			writeEmptyPrivateFile(t, path)
			if err := os.Mkdir(path+"-wal", 0o700); err != nil {
				t.Fatal(err)
			}
		}},
		{"WAL public mode", func(t *testing.T, path string) {
			writeEmptyPrivateFile(t, path)
			if err := os.WriteFile(path+"-wal", nil, 0o640); err != nil {
				t.Fatal(err)
			}
			if err := os.Chmod(path+"-wal", 0o640); err != nil {
				t.Fatal(err)
			}
		}},
		{"WAL preexisting owner mode", func(t *testing.T, path string) {
			writeEmptyPrivateFile(t, path)
			if err := os.WriteFile(path+"-wal", nil, 0); err != nil {
				t.Fatal(err)
			}
		}},
		{"SHM symlink", func(t *testing.T, path string) {
			writeEmptyPrivateFile(t, path)
			if err := os.Symlink(path, path+"-shm"); err != nil {
				t.Fatal(err)
			}
		}},
		{"SHM directory", func(t *testing.T, path string) {
			writeEmptyPrivateFile(t, path)
			if err := os.Mkdir(path+"-shm", 0o700); err != nil {
				t.Fatal(err)
			}
		}},
		{"SHM public mode", func(t *testing.T, path string) {
			writeEmptyPrivateFile(t, path)
			if err := os.WriteFile(path+"-shm", nil, 0o666); err != nil {
				t.Fatal(err)
			}
		}},
		{"SHM preexisting owner mode", func(t *testing.T, path string) {
			writeEmptyPrivateFile(t, path)
			if err := os.WriteFile(path+"-shm", nil, 0); err != nil {
				t.Fatal(err)
			}
		}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			parent := filepath.Join(t.TempDir(), "private")
			if err := os.Mkdir(parent, 0o700); err != nil {
				t.Fatal(err)
			}
			path := filepath.Join(parent, "unsafe-entry-marker.db")
			tc.setup(t, path)
			beforeModes := make(map[string]os.FileMode)
			for _, entry := range []string{path, path + "-wal", path + "-shm"} {
				if info, err := os.Lstat(entry); err == nil {
					beforeModes[entry] = info.Mode().Perm()
				}
			}
			_, err := Open(context.Background(), path, testOptions(t))
			assertSafeError(t, err, memory.ErrInvalidRequest, path, "unsafe-entry-marker")
			for entry, before := range beforeModes {
				after, statErr := os.Lstat(entry)
				if statErr == nil && before != after.Mode().Perm() {
					t.Fatalf("Open silently chmodded existing %s: %04o -> %04o", filepath.Base(entry), before, after.Mode().Perm())
				}
			}
		})
	}

	for _, entry := range []string{"database", "WAL", "SHM"} {
		if err := validateFileMetadata(syntheticMetadata{mode: regularMode | 0o600, uid: currentUID() + 1}, false); !errors.Is(err, memory.ErrInvalidRequest) {
			t.Fatalf("synthetic wrong-owner %s metadata = %v", entry, err)
		}
	}
}

func TestOpenRejectsConcurrentlyInsertedSidecarsWithoutModeRepair(t *testing.T) {
	if runtime.GOOS != "darwin" && runtime.GOOS != "linux" {
		t.Skip("secure path implementation is Unix-only")
	}

	t.Run("unsafe sidecar", func(t *testing.T) {
		root := t.TempDir()
		parent := filepath.Join(root, "private")
		if err := os.Mkdir(parent, 0o700); err != nil {
			t.Fatal(err)
		}
		path := filepath.Join(parent, "memory.db")
		installTestHooks(t, testHooks{path: func(event pathEvent) {
			if event == pathAfterPreflightDriverOpen {
				if err := os.WriteFile(path+"-wal", nil, 0o640); err != nil {
					panic(err)
				}
				if err := os.Chmod(path+"-wal", 0o640); err != nil {
					panic(err)
				}
			}
		}})

		_, err := Open(context.Background(), path, testOptions(t))
		assertSafeError(t, err, memory.ErrInvalidRequest, path)
		assertMode(t, path+"-wal", 0o640)
		assertNoPrivateDirectoryTemps(t, root)
	})

	t.Run("private sidecar is never normalized", func(t *testing.T) {
		root := t.TempDir()
		parent := filepath.Join(root, "private")
		if err := os.Mkdir(parent, 0o700); err != nil {
			t.Fatal(err)
		}
		path := filepath.Join(parent, "memory.db")
		var inserted *os.File
		installTestHooks(t, testHooks{path: func(event pathEvent) {
			if event != pathAfterPreflightDriverOpen || inserted != nil {
				return
			}
			var err error
			inserted, err = os.OpenFile(path+"-wal", os.O_RDWR|os.O_CREATE|os.O_EXCL, 0o400)
			if err != nil {
				panic(err)
			}
		}})

		_, err := Open(context.Background(), path, testOptions(t))
		if inserted != nil {
			defer inserted.Close()
		}
		assertSafeError(t, err, memory.ErrUnsupported, path)
		assertMode(t, path+"-wal", 0o400)
		assertNoPrivateDirectoryTemps(t, root)
	})
}

func TestOpenRejectsDatabaseSidecarInodeAliasing(t *testing.T) {
	if runtime.GOOS != "darwin" && runtime.GOOS != "linux" {
		t.Skip("secure path implementation is Unix-only")
	}
	for _, tc := range []struct {
		name string
		link func(path string) error
	}{
		{"database and WAL", func(path string) error { return os.Link(path, path+"-wal") }},
		{"database and SHM", func(path string) error { return os.Link(path, path+"-shm") }},
		{"WAL and SHM", func(path string) error {
			if err := os.WriteFile(path+"-wal", nil, 0o600); err != nil {
				return err
			}
			return os.Link(path+"-wal", path+"-shm")
		}},
		{"database external hardlink", func(path string) error { return os.Link(path, path+".alias") }},
		{"WAL external hardlink", func(path string) error {
			if err := os.WriteFile(path+"-wal", nil, 0o600); err != nil {
				return err
			}
			return os.Link(path+"-wal", path+".wal-alias")
		}},
		{"SHM external hardlink", func(path string) error {
			if err := os.WriteFile(path+"-shm", nil, 0o600); err != nil {
				return err
			}
			return os.Link(path+"-shm", path+".shm-alias")
		}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			parent := filepath.Join(t.TempDir(), "private")
			if err := os.Mkdir(parent, 0o700); err != nil {
				t.Fatal(err)
			}
			path := filepath.Join(parent, "memory.db")
			writeEmptyPrivateFile(t, path)
			if err := tc.link(path); err != nil {
				t.Fatal(err)
			}
			_, err := Open(context.Background(), path, testOptions(t))
			assertSafeError(t, err, memory.ErrInvalidRequest, path)
		})
	}
}

func TestOpenDescriptorRaceFaultsFailClosed(t *testing.T) {
	if runtime.GOOS != "darwin" && runtime.GOOS != "linux" {
		t.Skip("FD proof is Unix-only")
	}
	t.Run("database swapped after secure open", func(t *testing.T) {
		parent := filepath.Join(t.TempDir(), "parent")
		if err := os.Mkdir(parent, 0o700); err != nil {
			t.Fatal(err)
		}
		path := filepath.Join(parent, "race-marker.db")
		if err := os.WriteFile(path, nil, 0o600); err != nil {
			t.Fatal(err)
		}
		var once sync.Once
		installTestHooks(t, testHooks{path: func(event pathEvent) {
			if event == pathBeforePreflightDriverOpen {
				once.Do(func() {
					if err := os.Rename(path, path+".saved"); err != nil {
						panic(err)
					}
					if err := os.WriteFile(path, nil, 0o600); err != nil {
						panic(err)
					}
				})
			}
		}})
		_, err := Open(context.Background(), path, testOptions(t))
		assertSafeError(t, err, memory.ErrUnsupported, path, "race-marker")
	})

	t.Run("final parent swapped after first connection", func(t *testing.T) {
		root := t.TempDir()
		parent := filepath.Join(root, "parent")
		if err := os.Mkdir(parent, 0o700); err != nil {
			t.Fatal(err)
		}
		path := filepath.Join(parent, "memory.db")
		if err := os.WriteFile(path, nil, 0o600); err != nil {
			t.Fatal(err)
		}
		var once sync.Once
		installTestHooks(t, testHooks{path: func(event pathEvent) {
			if event == pathAfterPreflightDriverOpen {
				once.Do(func() {
					if err := os.Rename(parent, parent+".saved"); err != nil {
						panic(err)
					}
					if err := os.Mkdir(parent, 0o700); err != nil {
						panic(err)
					}
					if err := os.WriteFile(path, nil, 0o600); err != nil {
						panic(err)
					}
				})
			}
		}})
		_, err := Open(context.Background(), path, testOptions(t))
		assertSafeError(t, err, memory.ErrUnsupported, path)
	})

	t.Run("ancestor swapped before write connection", func(t *testing.T) {
		root := t.TempDir()
		ancestor := filepath.Join(root, "ancestor")
		parent := filepath.Join(ancestor, "parent")
		if err := os.MkdirAll(parent, 0o700); err != nil {
			t.Fatal(err)
		}
		path := filepath.Join(parent, "memory.db")
		if err := os.WriteFile(path, nil, 0o600); err != nil {
			t.Fatal(err)
		}
		var once sync.Once
		installTestHooks(t, testHooks{path: func(event pathEvent) {
			if event == pathBeforeWriteDriverOpen {
				once.Do(func() {
					if err := os.Rename(ancestor, ancestor+".saved"); err != nil {
						panic(err)
					}
					if err := os.MkdirAll(parent, 0o700); err != nil {
						panic(err)
					}
					if err := os.WriteFile(path, nil, 0o600); err != nil {
						panic(err)
					}
				})
			}
		}})
		_, err := Open(context.Background(), path, testOptions(t))
		assertSafeError(t, err, memory.ErrUnsupported, path)
	})

	t.Run("sqlite-open-only substitution is caught by FD inode proof", func(t *testing.T) {
		root := t.TempDir()
		parent := filepath.Join(root, "parent")
		if err := os.Mkdir(parent, 0o700); err != nil {
			t.Fatal(err)
		}
		path := filepath.Join(parent, "intended.db")
		if err := os.WriteFile(path, nil, 0o600); err != nil {
			t.Fatal(err)
		}
		target := filepath.Join(parent, "target.db")
		createRawFixture(t, target, `PRAGMA user_version=2`)
		before := fileDigest(t, target)
		installTestHooks(t, testHooks{path: func(event pathEvent) {
			switch event {
			case pathBeforePreflightDriverOpen:
				if err := os.Rename(path, path+".saved"); err != nil {
					panic(err)
				}
				if err := os.Rename(target, path); err != nil {
					panic(err)
				}
			case pathAfterPreflightDriverOpen:
				if err := os.Rename(path, target); err != nil {
					panic(err)
				}
				if err := os.Rename(path+".saved", path); err != nil {
					panic(err)
				}
			}
		}})
		_, err := Open(context.Background(), path, testOptions(t))
		assertSafeError(t, err, memory.ErrUnsupported, path, target)
		if after := fileDigest(t, target); after != before {
			t.Fatal("substitution target changed before proof failure")
		}
	})

	for _, tc := range []struct {
		name   string
		before pathEvent
		after  pathEvent
	}{
		{"retained connection 1", pathBeforeWriteDriverOpen, pathAfterWriteDriverOpen},
		{"retained connection 2", pathBeforeRetainedConnection2DriverOpen, pathAfterRetainedConnection2DriverOpen},
		{"retained connection 3", pathBeforeRetainedConnection3DriverOpen, pathAfterRetainedConnection3DriverOpen},
		{"retained connection 4", pathBeforeRetainedConnection4DriverOpen, pathAfterRetainedConnection4DriverOpen},
	} {
		t.Run(tc.name+" rejects substitution plus spoof FD", func(t *testing.T) {
			root := t.TempDir()
			path := filepath.Join(root, "intended", "memory.db")
			intended := openTestStore(t, path)
			if err := intended.Close(); err != nil {
				t.Fatal(err)
			}
			substitute := filepath.Join(root, "substitute", "memory.db")
			other := openTestStore(t, substitute)
			if err := other.Close(); err != nil {
				t.Fatal(err)
			}
			var spoof *os.File
			installTestHooks(t, testHooks{path: func(event pathEvent) {
				switch event {
				case tc.before:
					if err := os.Rename(path, path+".saved"); err != nil {
						panic(err)
					}
					if err := os.Rename(substitute, path); err != nil {
						panic(err)
					}
				case tc.after:
					if err := os.Rename(path, substitute); err != nil {
						panic(err)
					}
					if err := os.Rename(path+".saved", path); err != nil {
						panic(err)
					}
					var err error
					spoof, err = os.Open(path)
					if err != nil {
						panic(err)
					}
				}
			}})
			_, err := Open(context.Background(), path, Options{Guard: testGuard(t), NewID: memory.NewID})
			if spoof != nil {
				_ = spoof.Close()
			}
			assertSafeError(t, err, memory.ErrUnsupported, path, substitute)
			if got := processFDCountForPath(t, substitute); got != 0 {
				t.Fatalf("substitute inode FD count = %d after refusal", got)
			}
		})
	}

	t.Run("every unexpected newly opened regular FD is rejected", func(t *testing.T) {
		root := t.TempDir()
		parent := filepath.Join(root, "parent")
		if err := os.Mkdir(parent, 0o700); err != nil {
			t.Fatal(err)
		}
		path := filepath.Join(parent, "intended.db")
		writeEmptyPrivateFile(t, path)
		unrelatedPath := filepath.Join(root, "unrelated.db")
		writeEmptyPrivateFile(t, unrelatedPath)
		var unrelated *os.File
		installTestHooks(t, testHooks{path: func(event pathEvent) {
			if event == pathAfterPreflightDriverOpen {
				var err error
				unrelated, err = os.Open(unrelatedPath)
				if err != nil {
					panic(err)
				}
			}
		}})
		_, err := Open(context.Background(), path, testOptions(t))
		if unrelated != nil {
			_ = unrelated.Close()
		}
		assertSafeError(t, err, memory.ErrUnsupported, path, unrelatedPath)
	})

	t.Run("first connection rejects substitution plus spoof FD", func(t *testing.T) {
		root := t.TempDir()
		parent := filepath.Join(root, "parent")
		if err := os.Mkdir(parent, 0o700); err != nil {
			t.Fatal(err)
		}
		path := filepath.Join(parent, "intended.db")
		writeEmptyPrivateFile(t, path)
		substitute := filepath.Join(parent, "substitute.db")
		writeEmptyPrivateFile(t, substitute)
		var spoof *os.File
		installTestHooks(t, testHooks{path: func(event pathEvent) {
			switch event {
			case pathBeforePreflightDriverOpen:
				if err := os.Rename(path, path+".saved"); err != nil {
					panic(err)
				}
				if err := os.Rename(substitute, path); err != nil {
					panic(err)
				}
			case pathAfterPreflightDriverOpen:
				if err := os.Rename(path, substitute); err != nil {
					panic(err)
				}
				if err := os.Rename(path+".saved", path); err != nil {
					panic(err)
				}
				var err error
				spoof, err = os.Open(path)
				if err != nil {
					panic(err)
				}
			}
		}})
		_, err := Open(context.Background(), path, testOptions(t))
		if spoof != nil {
			_ = spoof.Close()
		}
		assertSafeError(t, err, memory.ErrUnsupported, path)
	})

	for _, stage := range []struct {
		name  string
		event pathEvent
	}{
		{"connection 2", pathAfterRetainedConnection2DriverOpen},
		{"connection 3", pathAfterRetainedConnection3DriverOpen},
		{"connection 4", pathAfterRetainedConnection4DriverOpen},
	} {
		for _, suffix := range []string{"-wal", "-shm"} {
			t.Run(stage.name+" "+strings.TrimPrefix(suffix, "-")+" stage swap", func(t *testing.T) {
				path := filepath.Join(t.TempDir(), "sidecar-stage", "memory.db")
				fixture := openTestStore(t, path)
				if err := fixture.Close(); err != nil {
					t.Fatal(err)
				}
				var swapped atomic.Bool
				installTestHooks(t, testHooks{path: func(event pathEvent) {
					if event != stage.event || !swapped.CompareAndSwap(false, true) {
						return
					}
					sidecar := path + suffix
					if err := os.Rename(sidecar, sidecar+".saved"); err != nil {
						panic(err)
					}
					if err := os.WriteFile(sidecar, nil, 0o600); err != nil {
						panic(err)
					}
				}})
				_, err := Open(context.Background(), path, Options{Guard: testGuard(t), NewID: memory.NewID})
				assertSafeError(t, err, memory.ErrUnsupported, path)
				if !swapped.Load() {
					t.Fatal("retained sidecar stage hook was not reached")
				}
			})
		}
	}

	for _, suffix := range []string{"-wal", "-shm"} {
		t.Run(strings.TrimPrefix(suffix, "-")+" swap after creation", func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "parent", "memory.db")
			var swapped atomic.Bool
			installTestHooks(t, testHooks{path: func(event pathEvent) {
				if event == pathAfterSidecarCreation && swapped.CompareAndSwap(false, true) {
					sidecar := path + suffix
					if _, err := os.Stat(sidecar); err == nil {
						if err := os.Rename(sidecar, sidecar+".saved"); err != nil {
							panic(err)
						}
						if err := os.WriteFile(sidecar, nil, 0o600); err != nil {
							panic(err)
						}
					}
				}
			}})
			_, err := Open(context.Background(), path, testOptions(t))
			assertSafeError(t, err, memory.ErrUnsupported, path)
			if !swapped.Load() {
				t.Fatal("sidecar hook was not reached")
			}
		})
	}
}

func TestOpenSerializesDelayedSidecarProofWithEveryAdapterFDOpen(t *testing.T) {
	root := t.TempDir()
	blocked := make(chan struct{})
	release := make(chan struct{})
	secondPreflight := make(chan struct{})
	secondProofAttempt := make(chan struct{})
	var blockedOnce sync.Once
	var secondOnce sync.Once
	var attemptOnce sync.Once
	installTestHooks(t, testHooks{beforeProofLock: func() {
		select {
		case <-blocked:
			attemptOnce.Do(func() { close(secondProofAttempt) })
		default:
		}
	}, path: func(event pathEvent) {
		switch event {
		case pathAfterSidecarCreation:
			blockedOnce.Do(func() {
				close(blocked)
				<-release
			})
		case pathBeforePreflightDriverOpen:
			select {
			case <-blocked:
				secondOnce.Do(func() { close(secondPreflight) })
			default:
			}
		}
	}})
	firstDone := make(chan error, 1)
	go func() {
		store, err := Open(context.Background(), filepath.Join(root, "first", "memory.db"), testOptions(t))
		if store != nil {
			_ = store.Close()
		}
		firstDone <- err
	}()
	<-blocked
	secondDone := make(chan error, 1)
	go func() {
		store, err := Open(context.Background(), filepath.Join(root, "second", "memory.db"), testOptions(t))
		if store != nil {
			_ = store.Close()
		}
		secondDone <- err
	}()
	<-secondProofAttempt
	select {
	case <-secondPreflight:
		t.Fatal("another adapter opened proof-stage descriptors during delayed sidecar proof")
	default:
	}
	close(release)
	if err := <-firstDone; err != nil {
		t.Fatal(err)
	}
	if err := <-secondDone; err != nil {
		t.Fatal(err)
	}
}

func TestOpenValidatesOptionsAndSafeErrors(t *testing.T) {
	path := filepath.Join(t.TempDir(), "sensitive-path-marker", "db?secret#.sqlite")
	cases := []struct {
		name string
		edit func(*Options)
	}{
		{"nil guard", func(o *Options) { o.Guard = nil }},
		{"bare default guard", func(o *Options) { o.Guard = memory.DefaultGuard{} }},
		{"bare custom guard", func(o *Options) { o.Guard = &receiptTrackingGuard{} }},
		{"nil composite guard", func(o *Options) { var guard *memory.CompositeGuard; o.Guard = guard }},
		{"negative timeout", func(o *Options) { o.BusyTimeout = -time.Millisecond }},
		{"overflowing timeout", func(o *Options) { o.BusyTimeout = (time.Duration(math.MaxInt32) + 1) * time.Millisecond }},
		{"ceil overflow timeout", func(o *Options) { o.BusyTimeout = time.Duration(math.MaxInt32)*time.Millisecond + time.Nanosecond }},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			o := testOptions(t)
			tc.edit(&o)
			_, err := Open(context.Background(), path, o)
			assertSafeError(t, err, memory.ErrInvalidRequest, path, "sensitive-path-marker", "db?secret#")
		})
	}

	t.Run("positive sub-millisecond timeout uses one millisecond everywhere", func(t *testing.T) {
		shortPath := filepath.Join(t.TempDir(), "submillisecond", "memory.db")
		store := openTestStore(t, shortPath, func(o *Options) { o.BusyTimeout = time.Nanosecond })
		conn := borrowTestConn(t, store)
		if got := queryInt(t, conn, "PRAGMA busy_timeout"); got != 1 {
			t.Fatalf("busy_timeout = %d, want exact ceil 1", got)
		}
	})

	t.Run("nil ID generator uses secure default", func(t *testing.T) {
		defaultPath := filepath.Join(t.TempDir(), "default-id", "memory.db")
		store, err := Open(context.Background(), defaultPath, Options{Guard: testGuard(t)})
		if err != nil {
			t.Fatal(err)
		}
		defer store.Close()
		identity, err := store.Identity(context.Background())
		if err != nil || !validDatabaseID(identity.DatabaseID) || !validDatabaseID(identity.UserScope.ID) {
			t.Fatalf("default identity = %#v, %v", identity, err)
		}
	})
}

func TestOpenBusyRetryAndTransactionAmbiguity(t *testing.T) {
	t.Run("external busy succeeds before budget", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "busy", "memory.db")
		store := openTestStore(t, path, func(o *Options) { o.BusyTimeout = 500 * time.Millisecond })
		raw := openRaw(t, path)
		defer raw.Close()
		if _, err := raw.Exec("BEGIN IMMEDIATE"); err != nil {
			t.Fatal(err)
		}
		attempted := make(chan struct{})
		var once sync.Once
		installTestHooks(t, testHooks{beforeBegin: func() { once.Do(func() { close(attempted) }) }})
		done := make(chan error, 1)
		go func() {
			done <- store.withWrite(context.Background(), memory.CommitSchema, nil, func(context.Context, *sql.Conn) error { return nil })
		}()
		<-attempted
		if _, err := raw.Exec("COMMIT"); err != nil {
			t.Fatal(err)
		}
		if err := <-done; err != nil {
			t.Fatalf("write after release: %v", err)
		}
	})

	t.Run("external busy is bounded", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "busy-timeout", "memory.db")
		store := openTestStore(t, path, func(o *Options) { o.BusyTimeout = 10 * time.Millisecond })
		raw := openRaw(t, path)
		defer raw.Close()
		if _, err := raw.Exec("BEGIN IMMEDIATE"); err != nil {
			t.Fatal(err)
		}
		start := time.Now()
		err := store.withWrite(context.Background(), memory.CommitSchema, nil, func(context.Context, *sql.Conn) error { return nil })
		elapsed := time.Since(start)
		assertSafeError(t, err, memory.ErrBusy, path)
		if elapsed > 250*time.Millisecond {
			t.Fatalf("busy timeout took %v", elapsed)
		}
		_, _ = raw.Exec("ROLLBACK")
	})

	t.Run("retry barrier and callback busy timeout is zero", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "cumulative-busy", "memory.db")
		store := openTestStore(t, path, func(o *Options) { o.BusyTimeout = 80 * time.Millisecond })
		retryReady := make(chan struct{})
		retryRelease := make(chan struct{})
		installTestHooks(t, testHooks{
			beginError: func(attempt int) error {
				if attempt == 0 {
					return sqliteCodeError{code: sqliteBusy}
				}
				return nil
			},
			retryDelay: func(context.Context, time.Duration) error {
				close(retryReady)
				<-retryRelease
				return nil
			},
		})
		done := make(chan error, 1)
		go func() {
			done <- store.withWrite(context.Background(), memory.CommitSchema, nil, func(ctx context.Context, conn *sql.Conn) error {
				var got int
				if err := conn.QueryRowContext(ctx, "PRAGMA busy_timeout").Scan(&got); err != nil {
					return err
				}
				if got != 0 {
					return fmt.Errorf("callback busy_timeout = %d", got)
				}
				return nil
			})
		}()
		<-retryReady
		close(retryRelease)
		if err := <-done; err != nil {
			t.Fatal(err)
		}
		conn := borrowTestConn(t, store)
		if got := queryInt(t, conn, "PRAGMA busy_timeout"); got != 80 {
			t.Fatalf("restored busy_timeout = %d", got)
		}
	})

	t.Run("only primary BUSY before callback retries", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "injected", "memory.db")
		store := openTestStore(t, path, func(o *Options) { o.BusyTimeout = 100 * time.Millisecond })
		var attempts atomic.Int64
		installTestHooks(t, testHooks{
			beginError: func(attempt int) error {
				attempts.Add(1)
				if attempt == 0 {
					return sqliteCodeError{code: sqliteBusy | (7 << 8)}
				}
				return nil
			},
			retryDelay: func(context.Context, time.Duration) error { return nil },
		})
		if err := store.withWrite(context.Background(), memory.CommitSchema, nil, func(context.Context, *sql.Conn) error { return nil }); err != nil {
			t.Fatal(err)
		}
		if attempts.Load() != 2 {
			t.Fatalf("attempts = %d", attempts.Load())
		}

		installTestHooks(t, testHooks{beginError: func(int) error { return sqliteCodeError{code: sqliteLocked} }})
		err := store.withWrite(context.Background(), memory.CommitSchema, nil, func(context.Context, *sql.Conn) error { return nil })
		assertSafeError(t, err, memory.ErrBusy)
	})

	t.Run("busy after callback is not retried", func(t *testing.T) {
		store := openTestStore(t, filepath.Join(t.TempDir(), "memory.db"))
		var calls atomic.Int64
		err := store.withWrite(context.Background(), memory.CommitSchema, nil, func(context.Context, *sql.Conn) error {
			calls.Add(1)
			return sqliteCodeError{code: sqliteBusy | (3 << 8)}
		})
		assertSafeError(t, err, memory.ErrBusy)
		if calls.Load() != 1 {
			t.Fatalf("callback calls = %d", calls.Load())
		}
	})

	t.Run("canceled before commit rolls back", func(t *testing.T) {
		store := openTestStore(t, filepath.Join(t.TempDir(), "memory.db"))
		ctx, cancel := context.WithCancel(context.Background())
		err := store.withWrite(ctx, memory.CommitSchema, nil, func(ctx context.Context, conn *sql.Conn) error {
			if _, err := conn.ExecContext(ctx, `INSERT INTO memory_meta(key,value) VALUES('temporary','value')`); err != nil {
				return err
			}
			cancel()
			return nil
		})
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("write = %v", err)
		}
		conn := borrowTestConn(t, store)
		if got := queryInt(t, conn, `SELECT count(*) FROM memory_meta WHERE key='temporary'`); got != 0 {
			t.Fatalf("row committed")
		}
	})

	t.Run("cancellation after commit point does not mask success", func(t *testing.T) {
		store := openTestStore(t, filepath.Join(t.TempDir(), "memory.db"))
		ctx, cancel := context.WithCancel(context.Background())
		installTestHooks(t, testHooks{commitStarted: cancel})
		err := store.withWrite(ctx, memory.CommitSchema, nil, func(ctx context.Context, conn *sql.Conn) error {
			_, err := conn.ExecContext(ctx, `INSERT INTO memory_meta(key,value) VALUES('committed','yes')`)
			return err
		})
		if err != nil {
			t.Fatalf("write = %v", err)
		}
	})

	t.Run("commit failure quarantines and poisons", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "commit-marker", "memory.db")
		store := openTestStore(t, path)
		quarantined := make(chan struct{}, 1)
		installTestHooks(t, testHooks{connectionQuarantined: func() { quarantined <- struct{}{} }, driverExec: func(statement string, exec func() error) error {
			if statement != "COMMIT" {
				return exec()
			}
			if err := exec(); err != nil {
				return err
			}
			return sqliteCodeError{code: 10}
		}})
		err := store.withWrite(context.Background(), memory.CommitUpsert, []string{"safe-id"}, func(ctx context.Context, conn *sql.Conn) error {
			if _, err := conn.ExecContext(ctx, `INSERT INTO memory_records(
				id,scope_namespace,scope_id,kind,semantic_key,text_value,labels_json,metadata_json,source_json,
				confidence,revision,created_at,updated_at,expires_at,state,forgotten_at
			) VALUES('safe-id','user','u','note','','commit-proof','[]','{}','{"origin":"human","session_id":"","message_ids":[],"observation_id":"","decision_at":null,"decision_source":""}',1,1,
				'2026-01-01T00:00:00.000000000Z','2026-01-01T00:00:00.000000000Z',NULL,'active',NULL)`); err != nil {
				return err
			}
			if _, err := conn.ExecContext(ctx, `INSERT INTO memory_records_fts(record_id,text_value,kind,semantic_key,labels) VALUES('safe-id','commit-proof','note','','')`); err != nil {
				return err
			}
			_, err := conn.ExecContext(ctx, `UPDATE memory_meta SET value='1' WHERE key='generation'`)
			return err
		})
		var unknown *memory.CommitUnknownError
		if !errors.As(err, &unknown) || !errors.Is(err, memory.ErrCommitUnknown) {
			t.Fatalf("write = %v", err)
		}
		if unknown.Operation() != memory.CommitUpsert || fmt.Sprint(unknown.EntityIDs()) != "[safe-id]" {
			t.Fatalf("commit metadata = %q %v", unknown.Operation(), unknown.EntityIDs())
		}
		if strings.Contains(err.Error(), path) || strings.Contains(err.Error(), "sqlite") || strings.Contains(err.Error(), "commit-proof") {
			t.Fatalf("unsafe commit error = %v", err)
		}
		<-quarantined
		if got := store.database.Stats().OpenConnections; got != retainedConnectionCount-1 {
			t.Fatalf("physical connections = %d", got)
		}
		if _, err := store.Identity(context.Background()); !errors.Is(err, memory.ErrUnavailable) {
			t.Fatalf("poisoned Identity = %v", err)
		}
		if len(store.connections) != retainedConnectionCount-1 {
			t.Fatalf("quarantined channel size = %d", len(store.connections))
		}
		if err := store.Close(); err != nil {
			t.Fatal(err)
		}
		if _, err := store.Identity(context.Background()); !errors.Is(err, memory.ErrClosed) {
			t.Fatalf("closed poisoned Identity = %v", err)
		}
		reopened, err := Open(context.Background(), path, Options{Guard: testGuard(t), NewID: memory.NewID})
		if err != nil {
			t.Fatalf("explicit reconciliation reopen: %v", err)
		}
		identity, identityErr := reopened.Identity(context.Background())
		if identityErr != nil || identity.Generation != 1 {
			t.Fatalf("reconciled generation = %d, %v", identity.Generation, identityErr)
		}
		conn := borrowTestConn(t, reopened)
		if got := queryString(t, conn, `SELECT text_value FROM memory_records WHERE id='safe-id'`); got != "commit-proof" {
			t.Fatalf("ID reconciliation value = %q", got)
		}
		_ = reopened.Close()
	})

	t.Run("rollback failure quarantines and poisons", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "memory.db")
		store := openTestStore(t, path)
		quarantined := make(chan struct{}, 1)
		installTestHooks(t, testHooks{connectionQuarantined: func() { quarantined <- struct{}{} }, driverExec: func(statement string, exec func() error) error {
			if statement != "ROLLBACK" {
				return exec()
			}
			if err := exec(); err != nil {
				return err
			}
			return sqliteCodeError{code: 10}
		}})
		err := store.withWrite(context.Background(), memory.CommitSchema, nil, func(ctx context.Context, conn *sql.Conn) error {
			if _, err := conn.ExecContext(ctx, `INSERT INTO memory_records(
				id,scope_namespace,scope_id,kind,semantic_key,text_value,labels_json,metadata_json,source_json,
				confidence,revision,created_at,updated_at,expires_at,state,forgotten_at
			) VALUES('rollback-proof','user','u','note','','must-disappear','[]','{}','{}',1,1,
				'2026-01-01T00:00:00.000000000Z','2026-01-01T00:00:00.000000000Z',NULL,'active',NULL)`); err != nil {
				return err
			}
			return errors.New("content-marker")
		})
		assertSafeError(t, err, memory.ErrUnavailable, "content-marker")
		if _, err := store.Identity(context.Background()); !errors.Is(err, memory.ErrUnavailable) {
			t.Fatalf("poisoned Identity = %v", err)
		}
		<-quarantined
		if got := store.database.Stats().OpenConnections; got != retainedConnectionCount-1 {
			t.Fatalf("physical connections = %d", got)
		}
		if err := store.Close(); err != nil {
			t.Fatal(err)
		}
		reopened, err := Open(context.Background(), path, Options{Guard: testGuard(t), NewID: memory.NewID})
		if err != nil {
			t.Fatal(err)
		}
		conn := borrowTestConn(t, reopened)
		if got := queryInt(t, conn, `SELECT count(*) FROM memory_records WHERE id='rollback-proof'`); got != 0 {
			t.Fatalf("real ROLLBACK left %d rows", got)
		}
		_ = reopened.Close()
	})
}

func TestOpenWriteGateCancellationAndPoisonWakeups(t *testing.T) {
	t.Run("write gate wait honors cancellation", func(t *testing.T) {
		store := openTestStore(t, filepath.Join(t.TempDir(), "gate-cancel", "memory.db"))
		<-store.writeGate
		ctx, cancel := context.WithCancel(context.Background())
		done := make(chan error, 1)
		go func() {
			done <- store.withWrite(ctx, memory.CommitSchema, nil, func(context.Context, *sql.Conn) error {
				return errors.New("callback must not run")
			})
		}()
		cancel()
		if err := <-done; !errors.Is(err, context.Canceled) {
			t.Fatalf("gate waiter = %v", err)
		}
		store.writeGate <- struct{}{}
	})

	t.Run("poison wakes write gate waiter", func(t *testing.T) {
		store := openTestStore(t, filepath.Join(t.TempDir(), "gate-poison", "memory.db"))
		<-store.writeGate
		done := make(chan error, 1)
		go func() {
			done <- store.withWrite(context.Background(), memory.CommitSchema, nil, func(context.Context, *sql.Conn) error {
				return errors.New("callback must not run")
			})
		}()
		store.poison()
		select {
		case err := <-done:
			if !errors.Is(err, memory.ErrUnavailable) {
				t.Fatalf("gate waiter = %v", err)
			}
		case <-time.After(time.Second):
			t.Fatal("poison did not wake gate waiter")
		}
		store.writeGate <- struct{}{}
	})

	t.Run("poison wakes connection waiter", func(t *testing.T) {
		store := openTestStore(t, filepath.Join(t.TempDir(), "connection-poison", "memory.db"))
		borrowed := make([]*sql.Conn, 0, retainedConnectionCount)
		for range retainedConnectionCount {
			borrowed = append(borrowed, <-store.connections)
		}
		done := make(chan error, 1)
		go func() {
			_, err := store.borrowConnection(context.Background())
			done <- err
		}()
		store.poison()
		select {
		case err := <-done:
			if !errors.Is(err, memory.ErrUnavailable) {
				t.Fatalf("connection waiter = %v", err)
			}
		case <-time.After(time.Second):
			t.Fatal("poison did not wake connection waiter")
		}
		for _, conn := range borrowed {
			_ = conn.Close()
		}
	})
}

func TestOpenSQLiteErrorClassificationUsesCodesNotMessages(t *testing.T) {
	canceled, cancel := context.WithCancel(context.Background())
	cancel()
	cases := []struct {
		name string
		ctx  context.Context
		err  error
		want error
	}{
		{"extended busy", context.Background(), sqliteCodeError{code: sqliteBusy | 7<<8}, memory.ErrBusy},
		{"locked", context.Background(), sqliteCodeError{code: sqliteLocked}, memory.ErrBusy},
		{"unique", context.Background(), sqliteCodeError{code: sqliteConstraintUnique}, memory.ErrConflict},
		{"primary key", context.Background(), sqliteCodeError{code: sqliteConstraintPrimaryKey}, memory.ErrConflict},
		{"check", context.Background(), sqliteCodeError{code: sqliteConstraintCheck}, memory.ErrCorrupt},
		{"not null", context.Background(), sqliteCodeError{code: sqliteConstraintNotNull}, memory.ErrCorrupt},
		{"foreign key", context.Background(), sqliteCodeError{code: sqliteConstraintForeignKey}, memory.ErrCorrupt},
		{"corrupt", context.Background(), sqliteCodeError{code: sqliteCorrupt}, memory.ErrCorrupt},
		{"not a database", context.Background(), sqliteCodeError{code: sqliteNotADB}, memory.ErrCorrupt},
		{"interrupt without cancellation", context.Background(), sqliteCodeError{code: sqliteInterrupt}, memory.ErrUnavailable},
		{"interrupt with cancellation", canceled, sqliteCodeError{code: sqliteInterrupt}, context.Canceled},
		{"closed connection", context.Background(), sql.ErrConnDone, memory.ErrClosed},
		{"arbitrary", context.Background(), errors.New("content-bearing-driver-message"), memory.ErrUnavailable},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got := safeSQLiteError(tc.ctx, tc.err)
			assertSafeError(t, got, tc.want, "content-bearing-driver-message")
		})
	}
}

func TestOpenSQLiteErrorClassificationPreservesTypedConflicts(t *testing.T) {
	conflict := &memory.ConflictError{EntityKind: "record", ID: "typed-conflict", ExpectedRevision: 1, ActualRevision: 2}
	if got := safeSQLiteError(context.Background(), conflict); got != conflict {
		t.Fatalf("safeSQLiteError conflict = %#v, want original %#v", got, conflict)
	}
	if got := safeRecordReadError(context.Background(), conflict); got != conflict {
		t.Fatalf("safeRecordReadError conflict = %#v, want original %#v", got, conflict)
	}

	store := openTestStore(t, filepath.Join(t.TempDir(), "memory.db"))
	err := store.withWrite(context.Background(), memory.CommitUpsert, []string{conflict.ID}, func(context.Context, *sql.Conn) error {
		return conflict
	})
	if err != conflict {
		t.Fatalf("withWrite conflict = %#v, want original %#v", err, conflict)
	}
}

func TestOpenCloseAdmissionIsConcurrentAndIdempotent(t *testing.T) {
	store := openTestStore(t, filepath.Join(t.TempDir(), "memory.db"))
	entered := make(chan struct{})
	release := make(chan struct{})
	operation := make(chan error, 1)
	go func() {
		operation <- store.withWrite(context.Background(), memory.CommitSchema, nil, func(context.Context, *sql.Conn) error {
			close(entered)
			<-release
			return nil
		})
	}()
	<-entered
	const closers = 10
	closeWaiting := make(chan struct{})
	var closeWaitingOnce sync.Once
	installTestHooks(t, testHooks{closeWait: func(bool) { closeWaitingOnce.Do(func() { close(closeWaiting) }) }})
	results := make(chan error, closers)
	for range closers {
		go func() { results <- store.Close() }()
	}
	<-closeWaiting
	close(release)
	if err := <-operation; err != nil {
		t.Fatal(err)
	}
	for range closers {
		if err := <-results; err != nil {
			t.Errorf("Close = %v", err)
		}
	}
	if _, err := store.Identity(context.Background()); !errors.Is(err, memory.ErrClosed) {
		t.Fatalf("Identity = %v", err)
	}
}

func TestOpenConcurrentCloseSharesSecureDescriptorError(t *testing.T) {
	for _, resource := range []string{"database-descriptor", "parent-descriptor"} {
		t.Run(resource, func(t *testing.T) {
			path := filepath.Join(t.TempDir(), "close-error", "memory.db")
			store, err := Open(context.Background(), path, testOptions(t))
			if err != nil {
				t.Fatal(err)
			}
			var injections atomic.Int64
			installTestHooks(t, testHooks{closeError: func(closedResource string, actual error) error {
				if actual != nil {
					return actual
				}
				if closedResource == resource && injections.Add(1) == 1 {
					return errors.New("injected descriptor close failure")
				}
				return nil
			}})
			const closers = 8
			start := make(chan struct{})
			results := make(chan error, closers)
			for range closers {
				go func() {
					<-start
					results <- store.Close()
				}()
			}
			close(start)
			for range closers {
				if err := <-results; !errors.Is(err, memory.ErrUnavailable) {
					t.Fatalf("shared Close result = %v", err)
				}
			}
			if got := injections.Load(); got != 1 {
				t.Fatalf("descriptor close injections = %d, want 1", got)
			}
			if err := store.Close(); !errors.Is(err, memory.ErrUnavailable) {
				t.Fatalf("idempotent Close result = %v", err)
			}
		})
	}
}

func fileDigest(t *testing.T, path string) [sha256.Size]byte {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return sha256.Sum256(data)
}

func writeEmptyPrivateFile(t *testing.T, path string) {
	t.Helper()
	if err := os.WriteFile(path, nil, 0o600); err != nil {
		t.Fatal(err)
	}
}

func processFDCountForPath(t *testing.T, path string) int {
	t.Helper()
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	stat, ok := info.Sys().(*syscall.Stat_t)
	if !ok {
		t.Fatalf("unsupported stat metadata %T", info.Sys())
	}
	fds, err := snapshotProcessFDs()
	if err != nil {
		t.Fatal(err)
	}
	return fds[inodeIdentity{device: uint64(stat.Dev), inode: uint64(stat.Ino)}]
}

func openRaw(t *testing.T, path string) *sql.DB {
	t.Helper()
	dsn, err := structuredFileURI(path, map[string]string{"mode": "rw"})
	if err != nil {
		t.Fatal(err)
	}
	db, err := sql.Open(driverName, dsn)
	if err != nil {
		t.Fatal(err)
	}
	if err := db.Ping(); err != nil {
		db.Close()
		t.Fatal(err)
	}
	return db
}

func createRawFixture(t *testing.T, path, statement string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		t.Fatal(err)
	}
	dsn, err := structuredFileURI(path, map[string]string{"mode": "rwc"})
	if err != nil {
		t.Fatal(err)
	}
	db, err := sql.Open(driverName, dsn)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := db.Exec(statement); err != nil {
		db.Close()
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(path, 0o600); err != nil {
		t.Fatal(err)
	}
}

func Example_structuredFileURI() {
	uri, _ := structuredFileURI("/private/memory ?#%.db", map[string]string{"mode": "ro"})
	fmt.Println(strings.HasPrefix(uri, "file:"), strings.Contains(uri, "%3F"), strings.Contains(uri, "%23"), strings.Contains(uri, "%25"))
	// Output: true true true true
}
