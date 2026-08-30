package sqlite

import (
	"context"
	"database/sql"
	"database/sql/driver"
	"errors"
	"math"
	"net/url"
	"strconv"
	"sync"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
	modernsqlite "modernc.org/sqlite"
)

const (
	driverName              = "sqlite"
	retainedConnectionCount = 4
	defaultBusyTimeout      = 5 * time.Second
)

const (
	sqliteLimitLength         = 0
	sqliteLimitSQLLength      = 1
	sqliteLimitColumn         = 2
	sqliteLimitExprDepth      = 3
	sqliteLimitCompoundSelect = 4
	sqliteLimitFunctionArg    = 6
	sqliteLimitAttached       = 7
	sqliteLimitVariableNumber = 9
	sqliteLimitTriggerDepth   = 10
	sqliteLimitWorkerThreads  = 11
)

var sqliteLimitCeilings = map[int]int{
	sqliteLimitLength:         131072,
	sqliteLimitSQLLength:      131072,
	sqliteLimitColumn:         64,
	sqliteLimitVariableNumber: 1024,
	sqliteLimitExprDepth:      100,
	sqliteLimitCompoundSelect: 50,
	sqliteLimitFunctionArg:    100,
	sqliteLimitAttached:       0,
	sqliteLimitTriggerDepth:   0,
	sqliteLimitWorkerThreads:  0,
}

type Options struct {
	BusyTimeout time.Duration
	NewID       func() (string, error)
	Guard       memory.ContentGuard
}

type storeState uint8

const (
	storeOpen storeState = iota
	storeClosing
	storeClosed
)

type Store struct {
	identity    memory.StoreIdentity
	busyTimeout time.Duration

	database    *sql.DB
	path        *securePath
	connections chan *sql.Conn
	allConns    []*sql.Conn
	writeGate   chan struct{}
	poisoned    chan struct{}
	poisonOnce  sync.Once

	lifecycleMu sync.Mutex
	lifecycle   *sync.Cond
	state       storeState
	active      int
	closeErr    error
}

type pathEvent uint8

const (
	pathBeforePreflightDriverOpen pathEvent = iota
	pathAfterPreflightDriverOpen
	pathAfterPreflightClose
	pathBeforeWriteDriverOpen
	pathAfterWriteDriverOpen
	pathBeforeRetainedConnection2DriverOpen
	pathAfterRetainedConnection2DriverOpen
	pathBeforeRetainedConnection3DriverOpen
	pathAfterRetainedConnection3DriverOpen
	pathBeforeRetainedConnection4DriverOpen
	pathAfterRetainedConnection4DriverOpen
	pathAfterSidecarCreation
)

type testHooks struct {
	path                   func(pathEvent)
	mkdirat                func(operation func() error) error
	beforeDirectoryInstall func(name string)
	storeReady             func(*Store)
	beforeBegin            func()
	beginError             func(attempt int) error
	retryDelay             func(context.Context, time.Duration) error
	commitStarted          func()
	driverExec             func(statement string, exec func() error) error
	closeError             func(resource string, actual error) error
}

var (
	hooksMu            sync.RWMutex
	installedHooks     testHooks
	connectionProofMu  sync.Mutex
	initializationGate = make(chan struct{}, 1)
)

func init() {
	initializationGate <- struct{}{}
}

func setTestHooks(hooks testHooks) func() {
	hooksMu.Lock()
	previous := installedHooks
	installedHooks = hooks
	hooksMu.Unlock()
	return func() {
		hooksMu.Lock()
		installedHooks = previous
		hooksMu.Unlock()
	}
}

func loadTestHooks() testHooks {
	hooksMu.RLock()
	hooks := installedHooks
	hooksMu.RUnlock()
	return hooks
}

func callPathHook(event pathEvent) {
	if hook := loadTestHooks().path; hook != nil {
		hook(event)
	}
}

func Open(ctx context.Context, filename string, options Options) (*Store, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if options.NewID == nil {
		options.NewID = memory.NewID
	}
	busyTimeout, err := validateOptions(options)
	if err != nil {
		return nil, err
	}

	// Secure descriptor acquisition is serialized with every proof-stage open,
	// so an independent adapter Open cannot contaminate another FD delta.
	connectionProofMu.Lock()
	path, err := openSecurePath(ctx, filename)
	connectionProofMu.Unlock()
	if err != nil {
		return nil, safeOpenError(ctx, err)
	}
	keepPath := false
	defer func() {
		if !keepPath {
			_ = path.close()
		}
	}()

	preflightDSN, err := structuredFileURI(path.canonicalPath, map[string]string{
		"mode":        "ro",
		"_query_only": "1",
		"_defensive":  "1",
		"_dqs":        "0",
		"_pragma":     "trusted_schema(OFF)",
	})
	if err != nil {
		return nil, err
	}
	preflightDB, err := sql.Open(driverName, preflightDSN)
	if err != nil {
		return nil, memory.ErrUnavailable
	}
	preflightDB.SetMaxOpenConns(1)
	preflightDB.SetMaxIdleConns(1)

	connectionProofMu.Lock()
	preflightConn, preflightBaseline, proofErr := openProvenPhysicalConnection(
		ctx, preflightDB, path, pathBeforePreflightDriverOpen, pathAfterPreflightDriverOpen,
	)
	var preflightIdentity memory.StoreIdentity
	var needsInitialization bool
	if proofErr == nil {
		preflightIdentity, needsInitialization, proofErr = inspectPreflight(ctx, preflightConn)
	}
	if proofErr == nil {
		proofErr = proveSQLiteSidecarsIfPresent(path, preflightBaseline)
	}
	if preflightConn != nil {
		if closeErr := preflightConn.Close(); proofErr == nil && closeErr != nil {
			proofErr = memory.ErrUnavailable
		}
	}
	if closeErr := preflightDB.Close(); proofErr == nil && closeErr != nil {
		proofErr = memory.ErrUnavailable
	}
	if proofErr == nil {
		proofErr = path.revalidate()
	}
	connectionProofMu.Unlock()
	if proofErr != nil {
		return nil, safeOpenError(ctx, proofErr)
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	callPathHook(pathAfterPreflightClose)
	if err := ctx.Err(); err != nil {
		return nil, err
	}

	databaseID, userID := preflightIdentity.DatabaseID, preflightIdentity.UserScope.ID
	if needsInitialization {
		ids, err := generateProspectiveIDs(options.NewID)
		if err != nil {
			return nil, safeOpenError(ctx, err)
		}
		databaseID, userID = ids[0], ids[1]
		if err := guardIdentity(ctx, options.Guard, databaseID, userID); err != nil {
			return nil, err
		}
	} else if err := guardStoreIdentity(ctx, options.Guard, preflightIdentity); err != nil {
		return nil, err
	}

	busyMilliseconds, err := ceilMilliseconds(busyTimeout)
	if err != nil {
		return nil, err
	}
	writeDSN, err := structuredFileURI(path.canonicalPath, map[string]string{
		"mode":          "rw",
		"_defensive":    "1",
		"_foreign_keys": "on",
		"_synchronous":  "FULL",
		"_busy_timeout": strconv.FormatInt(busyMilliseconds, 10),
		"_txlock":       "immediate",
		"_dqs":          "0",
		"_pragma":       "trusted_schema(OFF)",
	})
	if err != nil {
		return nil, err
	}
	database, err := sql.Open(driverName, writeDSN)
	if err != nil {
		return nil, memory.ErrUnavailable
	}
	database.SetMaxOpenConns(retainedConnectionCount)
	database.SetMaxIdleConns(retainedConnectionCount)
	database.SetConnMaxLifetime(0)
	database.SetConnMaxIdleTime(0)
	connections := make([]*sql.Conn, 0, retainedConnectionCount)
	cleanupConnections := func() {
		for _, conn := range connections {
			_ = conn.Close()
		}
		_ = database.Close()
	}

	select {
	case <-ctx.Done():
		cleanupConnections()
		return nil, ctx.Err()
	case <-initializationGate:
	}
	identity, initializationErr := func() (memory.StoreIdentity, error) {
		defer func() { initializationGate <- struct{}{} }()
		connectionProofMu.Lock()
		defer connectionProofMu.Unlock()

		firstConn, _, err := openProvenPhysicalConnection(
			ctx, database, path, pathBeforeWriteDriverOpen, pathAfterWriteDriverOpen,
		)
		if err != nil {
			return memory.StoreIdentity{}, err
		}
		connections = append(connections, firstConn)
		if err := configureConnection(firstConn, busyTimeout); err != nil {
			return memory.StoreIdentity{}, err
		}
		if err := recheckPreflight(ctx, firstConn, preflightIdentity, needsInitialization); err != nil {
			return memory.StoreIdentity{}, err
		}
		if err := path.revalidate(); err != nil {
			return memory.StoreIdentity{}, err
		}
		_, hadWAL := path.sidecarIdentities()["-wal"]
		persistentBaseline, err := snapshotProcessFDs()
		if err != nil {
			return memory.StoreIdentity{}, err
		}
		var mode string
		if err := firstConn.QueryRowContext(ctx, "PRAGMA journal_mode=WAL").Scan(&mode); err != nil {
			return memory.StoreIdentity{}, safeSQLiteError(ctx, err)
		}
		if mode != "wal" && mode != "WAL" {
			return memory.StoreIdentity{}, memory.ErrUnavailable
		}
		identity, err := initializeOrVerifySchema(ctx, firstConn, databaseID, userID)
		if err != nil {
			return memory.StoreIdentity{}, err
		}
		callPathHook(pathAfterSidecarCreation)
		if err := proveSQLiteSidecars(path, persistentBaseline, !hadWAL); err != nil {
			return memory.StoreIdentity{}, err
		}
		return identity, nil
	}()
	if initializationErr != nil {
		cleanupConnections()
		return nil, safeOpenError(ctx, initializationErr)
	}
	if err := ctx.Err(); err != nil {
		cleanupConnections()
		return nil, err
	}

	retainedEvents := [...][2]pathEvent{
		{pathBeforeRetainedConnection2DriverOpen, pathAfterRetainedConnection2DriverOpen},
		{pathBeforeRetainedConnection3DriverOpen, pathAfterRetainedConnection3DriverOpen},
		{pathBeforeRetainedConnection4DriverOpen, pathAfterRetainedConnection4DriverOpen},
	}
	for index := 0; len(connections) < retainedConnectionCount; index++ {
		connectionProofMu.Lock()
		conn, baseline, err := openProvenPhysicalConnection(ctx, database, path, retainedEvents[index][0], retainedEvents[index][1])
		if err == nil {
			connections = append(connections, conn)
			err = configureConnection(conn, busyTimeout)
		}
		if err == nil {
			var got memory.StoreIdentity
			got, err = verifySchema(ctx, conn)
			if err == nil && got != identity {
				err = memory.ErrCorrupt
			}
		}
		if err == nil {
			err = proveRetainedSQLiteConnection(path, baseline)
		}
		connectionProofMu.Unlock()
		if err != nil {
			cleanupConnections()
			return nil, safeOpenError(ctx, err)
		}
		if err := ctx.Err(); err != nil {
			cleanupConnections()
			return nil, err
		}
	}

	store := &Store{
		identity:    identity,
		busyTimeout: busyTimeout,
		database:    database,
		path:        path,
		connections: make(chan *sql.Conn, retainedConnectionCount),
		allConns:    connections,
		writeGate:   make(chan struct{}, 1),
		poisoned:    make(chan struct{}),
		state:       storeOpen,
	}
	store.lifecycle = sync.NewCond(&store.lifecycleMu)
	store.writeGate <- struct{}{}
	for _, conn := range connections {
		store.connections <- conn
	}
	keepPath = true
	if hook := loadTestHooks().storeReady; hook != nil {
		hook(store)
	}
	if err := guardStoreIdentity(ctx, options.Guard, identity); err != nil {
		_ = store.Close()
		return nil, err
	}
	if err := ctx.Err(); err != nil {
		_ = store.Close()
		return nil, err
	}
	return store, nil
}

func recheckPreflight(ctx context.Context, conn *sql.Conn, before memory.StoreIdentity, beforeNeedsInitialization bool) error {
	identity, needsInitialization, err := inspectPreflight(ctx, conn)
	if err != nil {
		return err
	}
	if needsInitialization != beforeNeedsInitialization {
		return memory.ErrCorrupt
	}
	if !needsInitialization && identity != before {
		return memory.ErrCorrupt
	}
	return nil
}

func validateOptions(options Options) (time.Duration, error) {
	if options.Guard == nil {
		return 0, memory.ErrInvalidRequest
	}
	timeout := options.BusyTimeout
	if timeout == 0 {
		timeout = defaultBusyTimeout
	}
	milliseconds, err := ceilMilliseconds(timeout)
	if err != nil {
		return 0, err
	}
	return time.Duration(milliseconds) * time.Millisecond, nil
}

func ceilMilliseconds(duration time.Duration) (int64, error) {
	if duration <= 0 {
		return 0, memory.ErrInvalidRequest
	}
	milliseconds := int64(duration / time.Millisecond)
	if duration%time.Millisecond != 0 {
		milliseconds++
	}
	if milliseconds <= 0 || milliseconds > math.MaxInt32 {
		return 0, memory.ErrInvalidRequest
	}
	return milliseconds, nil
}

func structuredFileURI(path string, parameters map[string]string) (string, error) {
	if path == "" {
		return "", memory.ErrInvalidRequest
	}
	u := &url.URL{Scheme: "file", Path: path}
	query := url.Values{}
	for key, value := range parameters {
		if key == "" {
			return "", memory.ErrInvalidRequest
		}
		query.Set(key, value)
	}
	u.RawQuery = query.Encode()
	return u.String(), nil
}

// openProvenPhysicalConnection must be called while connectionProofMu is held.
// Its baseline immediately precedes the physical driver open, and every
// positive regular-FD delta is checked by proveSQLiteConnection.
func openProvenPhysicalConnection(ctx context.Context, database *sql.DB, path *securePath, beforeEvent, afterEvent pathEvent) (*sql.Conn, map[inodeIdentity]int, error) {
	callPathHook(beforeEvent)
	before, err := snapshotProcessFDs()
	if err != nil {
		return nil, nil, err
	}
	conn, err := database.Conn(context.Background())
	if err != nil {
		return nil, nil, safeOpenError(ctx, err)
	}
	callPathHook(afterEvent)
	if err := applyLimits(conn); err != nil {
		_ = conn.Close()
		return nil, nil, err
	}
	if err := proveSQLiteConnection(context.Background(), conn, before, path); err != nil {
		_ = conn.Close()
		return nil, nil, safeOpenError(ctx, err)
	}
	if err := ctx.Err(); err != nil {
		_ = conn.Close()
		return nil, nil, err
	}
	return conn, before, nil
}

func applyLimits(conn *sql.Conn) error {
	for limit, ceiling := range sqliteLimitCeilings {
		if _, err := connectionLimit(conn, limit, ceiling); err != nil {
			return memory.ErrUnavailable
		}
		actual, err := connectionLimit(conn, limit, -1)
		if err != nil || actual != ceiling {
			return memory.ErrUnsupported
		}
	}
	return nil
}

func connectionLimit(conn *sql.Conn, limit, value int) (int, error) {
	return modernsqlite.Limit(conn, limit, value)
}

func configureConnection(conn *sql.Conn, busyTimeout time.Duration) error {
	if err := applyLimits(conn); err != nil {
		return err
	}
	busyMilliseconds, err := ceilMilliseconds(busyTimeout)
	if err != nil {
		return err
	}
	statements := []string{
		"PRAGMA foreign_keys=ON",
		"PRAGMA synchronous=FULL",
		"PRAGMA busy_timeout=" + strconv.FormatInt(busyMilliseconds, 10),
		"PRAGMA trusted_schema=OFF",
		"PRAGMA writable_schema=OFF",
	}
	for _, statement := range statements {
		if _, err := conn.ExecContext(context.Background(), statement); err != nil {
			return memory.ErrUnavailable
		}
	}
	checks := []struct {
		query string
		want  int
	}{
		{"PRAGMA foreign_keys", 1},
		{"PRAGMA synchronous", 2},
		{"PRAGMA busy_timeout", int(busyMilliseconds)},
		{"PRAGMA trusted_schema", 0},
		{"PRAGMA writable_schema", 0},
	}
	for _, check := range checks {
		var got int
		if err := conn.QueryRowContext(context.Background(), check.query).Scan(&got); err != nil || got != check.want {
			return memory.ErrUnsupported
		}
	}
	if _, err := conn.ExecContext(context.Background(), `SELECT "DQS fallback is disabled"`); err == nil {
		return memory.ErrUnsupported
	}
	return nil
}

func generateProspectiveIDs(generate func() (string, error)) ([2]string, error) {
	var ids [2]string
	for index := range ids {
		for duplicateRetries := 0; ; duplicateRetries++ {
			id, err := generate()
			if err != nil {
				return [2]string{}, memory.ErrUnavailable
			}
			if !validDatabaseID(id) {
				return [2]string{}, memory.ErrInvalidRequest
			}
			if index == 1 && id == ids[0] {
				if duplicateRetries == memory.MaxDuplicateIDRetries {
					return [2]string{}, memory.ErrUnavailable
				}
				continue
			}
			ids[index] = id
			break
		}
	}
	return ids, nil
}

func guardIdentity(ctx context.Context, guard memory.ContentGuard, databaseID, userID string) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	err := guard.Check(ctx, memory.GuardInput{Fields: []memory.GuardField{
		{Name: "database ID", Value: databaseID, Opaque: true},
		{Name: "user scope ID", Value: userID, Opaque: true},
	}})
	if err == nil {
		return nil
	}
	if ctxErr := ctx.Err(); ctxErr != nil {
		return ctxErr
	}
	switch {
	case errors.Is(err, memory.ErrSensitiveMemory):
		return memory.ErrSensitiveMemory
	case errors.Is(err, memory.ErrInvalidRequest):
		return memory.ErrInvalidRequest
	default:
		return memory.ErrUnavailable
	}
}

func guardStoreIdentity(ctx context.Context, guard memory.ContentGuard, identity memory.StoreIdentity) error {
	return guardIdentity(ctx, guard, identity.DatabaseID, identity.UserScope.ID)
}

func safeOpenError(ctx context.Context, err error) error {
	if err == nil {
		return nil
	}
	if ctxErr := ctx.Err(); ctxErr != nil {
		return ctxErr
	}
	for _, category := range []error{
		context.Canceled,
		context.DeadlineExceeded,
		memory.ErrInvalidRequest,
		memory.ErrSensitiveMemory,
		memory.ErrUnsupported,
		memory.ErrIncompatibleSchema,
		memory.ErrCorrupt,
		memory.ErrBusy,
		memory.ErrCommitUnknown,
		memory.ErrUnavailable,
	} {
		if errors.Is(err, category) {
			return category
		}
	}
	return memory.ErrUnavailable
}

func (store *Store) Identity(ctx context.Context) (memory.StoreIdentity, error) {
	if err := ctx.Err(); err != nil {
		return memory.StoreIdentity{}, err
	}
	done, err := store.admit()
	if err != nil {
		return memory.StoreIdentity{}, err
	}
	defer done()
	return store.identity, nil
}

func (store *Store) admit() (func(), error) {
	if store == nil {
		return nil, memory.ErrClosed
	}
	store.lifecycleMu.Lock()
	defer store.lifecycleMu.Unlock()
	if store.state != storeOpen {
		return nil, memory.ErrClosed
	}
	select {
	case <-store.poisoned:
		return nil, memory.ErrUnavailable
	default:
	}
	store.active++
	return func() {
		store.lifecycleMu.Lock()
		store.active--
		if store.active == 0 {
			store.lifecycle.Broadcast()
		}
		store.lifecycleMu.Unlock()
	}, nil
}

func (store *Store) Close() error {
	if store == nil {
		return nil
	}
	store.lifecycleMu.Lock()
	switch store.state {
	case storeClosing:
		for store.state == storeClosing {
			store.lifecycle.Wait()
		}
		err := store.closeErr
		store.lifecycleMu.Unlock()
		return err
	case storeClosed:
		err := store.closeErr
		store.lifecycleMu.Unlock()
		return err
	}
	store.state = storeClosing
	for store.active != 0 {
		store.lifecycle.Wait()
	}
	store.lifecycleMu.Unlock()

	var closeFailed bool
	for _, conn := range store.allConns {
		if err := conn.Close(); err != nil && !errors.Is(err, sql.ErrConnDone) && !errors.Is(err, driver.ErrBadConn) {
			closeFailed = true
		}
	}
	if err := store.database.Close(); err != nil {
		closeFailed = true
	}
	if err := store.path.close(); err != nil {
		closeFailed = true
	}
	var result error
	if closeFailed {
		result = memory.ErrUnavailable
	}

	store.lifecycleMu.Lock()
	store.closeErr = result
	store.state = storeClosed
	store.lifecycle.Broadcast()
	store.lifecycleMu.Unlock()
	return result
}

func (store *Store) poison() {
	store.poisonOnce.Do(func() { close(store.poisoned) })
}
