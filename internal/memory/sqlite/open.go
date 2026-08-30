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
	pathBeforeWriteDriverOpen
	pathAfterWriteDriverOpen
	pathAfterSidecarCreation
)

type testHooks struct {
	path          func(pathEvent)
	beforeBegin   func()
	beginError    func(attempt int) error
	retryDelay    func(context.Context, time.Duration) error
	commitStarted func()
	commitError   func() error
	rollbackError func() error
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
	path, err := openSecurePath(ctx, filename)
	if err != nil {
		return nil, safeOpenError(ctx, err)
	}
	keepPath := false
	defer func() {
		if !keepPath {
			path.close()
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
	preflightDB, preflightConn, preflightBaseline, err := openProvenConnection(ctx, preflightDSN, path, pathBeforePreflightDriverOpen, pathAfterPreflightDriverOpen)
	if err != nil {
		return nil, err
	}
	if err := applyLimits(preflightConn); err != nil {
		preflightConn.Close()
		preflightDB.Close()
		return nil, err
	}
	preflightIdentity, needsInitialization, inspectErr := inspectPreflight(ctx, preflightConn)
	if proofErr := proveSQLiteSidecarsIfPresent(path, preflightBaseline); proofErr != nil {
		inspectErr = proofErr
	} else if validateErr := path.validateSidecarEntries(); inspectErr == nil && validateErr != nil {
		inspectErr = validateErr
	}
	closeErr := preflightConn.Close()
	if dbErr := preflightDB.Close(); closeErr == nil {
		closeErr = dbErr
	}
	if inspectErr != nil {
		return nil, safeOpenError(ctx, inspectErr)
	}
	if closeErr != nil {
		return nil, memory.ErrUnavailable
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if err := path.revalidate(); err != nil {
		return nil, safeOpenError(ctx, err)
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

	writeDSN, err := structuredFileURI(path.canonicalPath, map[string]string{
		"mode":          "rw",
		"_defensive":    "1",
		"_foreign_keys": "on",
		"_synchronous":  "FULL",
		"_busy_timeout": strconv.FormatInt(busyTimeout.Milliseconds(), 10),
		"_txlock":       "immediate",
		"_dqs":          "0",
		"_pragma":       "trusted_schema(OFF)",
	})
	if err != nil {
		return nil, err
	}
	database, firstConn, sidecarBaseline, err := openProvenConnection(ctx, writeDSN, path, pathBeforeWriteDriverOpen, pathAfterWriteDriverOpen)
	if err != nil {
		return nil, err
	}
	database.SetMaxOpenConns(retainedConnectionCount)
	database.SetMaxIdleConns(retainedConnectionCount)
	database.SetConnMaxLifetime(0)
	database.SetConnMaxIdleTime(0)
	connections := []*sql.Conn{firstConn}
	cleanupConnections := func() {
		for _, conn := range connections {
			_ = conn.Close()
		}
		_ = database.Close()
	}
	if err := configureConnection(firstConn, busyTimeout); err != nil {
		cleanupConnections()
		return nil, err
	}
	if err := ctx.Err(); err != nil {
		cleanupConnections()
		return nil, err
	}

	select {
	case <-ctx.Done():
		cleanupConnections()
		return nil, ctx.Err()
	case <-initializationGate:
	}
	identity, initializationErr := func() (memory.StoreIdentity, error) {
		defer func() { initializationGate <- struct{}{} }()
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
		if err := proveSQLiteSidecars(path, sidecarBaseline); err != nil {
			return memory.StoreIdentity{}, err
		}
		if err := path.revalidate(); err != nil {
			return memory.StoreIdentity{}, err
		}
		return identity, nil
	}()
	if initializationErr != nil {
		cleanupConnections()
		return nil, safeOpenError(ctx, initializationErr)
	}
	if err := guardStoreIdentity(ctx, options.Guard, identity); err != nil {
		cleanupConnections()
		return nil, err
	}
	if err := ctx.Err(); err != nil {
		cleanupConnections()
		return nil, err
	}

	for len(connections) < retainedConnectionCount {
		conn, err := database.Conn(context.Background())
		if err != nil {
			cleanupConnections()
			return nil, safeOpenError(ctx, err)
		}
		connections = append(connections, conn)
		if err := configureConnection(conn, busyTimeout); err != nil {
			cleanupConnections()
			return nil, err
		}
		if err := ctx.Err(); err != nil {
			cleanupConnections()
			return nil, err
		}
		if err := path.revalidate(); err != nil {
			cleanupConnections()
			return nil, safeOpenError(ctx, err)
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
	return store, nil
}

func validateOptions(options Options) (time.Duration, error) {
	if options.Guard == nil {
		return 0, memory.ErrInvalidRequest
	}
	timeout := options.BusyTimeout
	if timeout == 0 {
		timeout = defaultBusyTimeout
	}
	if timeout < 0 || timeout/time.Millisecond > math.MaxInt32 {
		return 0, memory.ErrInvalidRequest
	}
	return timeout, nil
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

func openProvenConnection(ctx context.Context, dsn string, path *securePath, beforeEvent, afterEvent pathEvent) (*sql.DB, *sql.Conn, map[inodeIdentity]int, error) {
	callPathHook(beforeEvent)
	connectionProofMu.Lock()
	defer connectionProofMu.Unlock()
	before, err := snapshotProcessFDs()
	if err != nil {
		return nil, nil, nil, err
	}
	database, err := sql.Open(driverName, dsn)
	if err != nil {
		return nil, nil, nil, memory.ErrUnavailable
	}
	database.SetMaxOpenConns(1)
	database.SetMaxIdleConns(1)
	conn, err := database.Conn(context.Background())
	if err != nil {
		database.Close()
		return nil, nil, nil, safeOpenError(ctx, err)
	}
	callPathHook(afterEvent)
	if err := applyLimits(conn); err != nil {
		conn.Close()
		database.Close()
		return nil, nil, nil, err
	}
	if err := proveSQLiteConnection(context.Background(), conn, before, path); err != nil {
		conn.Close()
		database.Close()
		return nil, nil, nil, safeOpenError(ctx, err)
	}
	if err := ctx.Err(); err != nil {
		conn.Close()
		database.Close()
		return nil, nil, nil, err
	}
	return database, conn, before, nil
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
	statements := []string{
		"PRAGMA foreign_keys=ON",
		"PRAGMA synchronous=FULL",
		"PRAGMA busy_timeout=" + strconv.FormatInt(busyTimeout.Milliseconds(), 10),
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
		{"PRAGMA busy_timeout", int(busyTimeout.Milliseconds())},
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
	store.path.close()
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
