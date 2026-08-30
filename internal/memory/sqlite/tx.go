package sqlite

import (
	"context"
	"database/sql"
	"database/sql/driver"
	"errors"
	"strconv"
	"time"

	"github.com/baiyuqing/otto/internal/memory"
	modernsqlite "modernc.org/sqlite"
)

const (
	sqliteBusy       = 5
	sqliteLocked     = 6
	sqliteInterrupt  = 9
	sqliteCorrupt    = 11
	sqliteTooBig     = 18
	sqliteConstraint = 19
	sqliteMisuse     = 21
	sqliteNotADB     = 26

	sqliteConstraintCheck      = 275
	sqliteConstraintForeignKey = 787
	sqliteConstraintNotNull    = 1299
	sqliteConstraintPrimaryKey = 1555
	sqliteConstraintUnique     = 2067
)

type sqliteCodeError struct{ code int }

func (err sqliteCodeError) Error() string { return memory.ErrUnavailable.Error() }
func (err sqliteCodeError) Code() int     { return err.code }

type sqliteCoder interface{ Code() int }

func safeSQLiteError(ctx context.Context, err error) error {
	if err == nil {
		return nil
	}
	if ctx != nil {
		if ctxErr := ctx.Err(); ctxErr != nil {
			return ctxErr
		}
	}
	if errors.Is(err, sql.ErrConnDone) || errors.Is(err, driver.ErrBadConn) {
		return memory.ErrClosed
	}
	var conflict *memory.ConflictError
	if errors.As(err, &conflict) {
		return conflict
	}
	for _, category := range []error{
		memory.ErrInvalidRequest, memory.ErrInvalidRecord, memory.ErrSensitiveMemory, memory.ErrUnsupported,
		memory.ErrNotFound, memory.ErrConflict, memory.ErrCorrupt, memory.ErrBusy, memory.ErrClosed, memory.ErrUnavailable,
	} {
		if errors.Is(err, category) {
			return category
		}
	}
	var modernError *modernsqlite.Error
	var coded sqliteCoder
	var code int
	switch {
	case errors.As(err, &modernError):
		code = modernError.Code()
	case errors.As(err, &coded):
		code = coded.Code()
	default:
		if errors.Is(err, context.Canceled) {
			return context.Canceled
		}
		if errors.Is(err, context.DeadlineExceeded) {
			return context.DeadlineExceeded
		}
		return memory.ErrUnavailable
	}
	primary := code & 0xff
	switch code {
	case sqliteConstraintUnique, sqliteConstraintPrimaryKey:
		return memory.ErrConflict
	case sqliteConstraintCheck, sqliteConstraintNotNull, sqliteConstraintForeignKey:
		return memory.ErrCorrupt
	}
	switch primary {
	case sqliteBusy, sqliteLocked:
		return memory.ErrBusy
	case sqliteCorrupt, sqliteNotADB:
		return memory.ErrCorrupt
	case sqliteInterrupt:
		if ctx != nil && ctx.Err() != nil {
			return ctx.Err()
		}
		return memory.ErrUnavailable
	case sqliteMisuse:
		return memory.ErrClosed
	case sqliteConstraint:
		return memory.ErrCorrupt
	default:
		return memory.ErrUnavailable
	}
}

func sqlitePrimaryCode(err error) int {
	var modernError *modernsqlite.Error
	if errors.As(err, &modernError) {
		return modernError.Code() & 0xff
	}
	var coded sqliteCoder
	if errors.As(err, &coded) {
		return coded.Code() & 0xff
	}
	return -1
}

func (store *Store) withWrite(
	ctx context.Context,
	operation memory.CommitOperation,
	entityIDs []string,
	callback func(context.Context, *sql.Conn) error,
) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	if callback == nil {
		return memory.ErrInvalidRequest
	}
	commitUnknown, err := memory.NewCommitUnknownError(operation, entityIDs)
	if err != nil {
		return memory.ErrInvalidRequest
	}
	done, err := store.continueOperation(ctx)
	if err != nil {
		return err
	}
	defer done()

	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-store.poisoned:
		return memory.ErrUnavailable
	case <-store.writeGate:
	}
	defer func() { store.writeGate <- struct{}{} }()

	started := time.Now()
	deadline := started.Add(store.busyTimeout)
	seed := uint64(started.UnixNano()) | 1
	attempt := 0
	for {
		if err := ctx.Err(); err != nil {
			return err
		}
		select {
		case <-store.poisoned:
			return memory.ErrUnavailable
		default:
		}
		remaining := time.Until(deadline)
		if remaining <= 0 {
			return memory.ErrBusy
		}
		conn, err := store.borrowConnection(ctx)
		if err != nil {
			return err
		}
		milliseconds, conversionErr := ceilMilliseconds(remaining)
		if conversionErr != nil {
			store.returnConnection(conn)
			return memory.ErrBusy
		}
		if _, err := conn.ExecContext(context.Background(), "PRAGMA busy_timeout="+strconv.FormatInt(milliseconds, 10)); err != nil {
			store.quarantine(conn)
			return memory.ErrUnavailable
		}

		hooks := loadTestHooks()
		if hooks.beforeBegin != nil {
			hooks.beforeBegin()
		}
		var beginErr error
		if hooks.beginError != nil {
			beginErr = hooks.beginError(attempt)
		}
		if beginErr == nil {
			_, beginErr = conn.ExecContext(ctx, "BEGIN IMMEDIATE")
		}
		if beginErr != nil {
			if err := store.restoreAndReturnConnection(conn); err != nil {
				return err
			}
			primary := sqlitePrimaryCode(beginErr)
			if primary == sqliteLocked {
				return memory.ErrBusy
			}
			if primary != sqliteBusy {
				return safeSQLiteError(ctx, beginErr)
			}
			if time.Until(deadline) <= 0 {
				return memory.ErrBusy
			}
			delay := retryBackoff(attempt, &seed)
			if remaining := time.Until(deadline); delay > remaining {
				delay = remaining
			}
			attempt++
			if hooks.retryDelay != nil {
				if err := hooks.retryDelay(ctx, delay); err != nil {
					if ctxErr := ctx.Err(); ctxErr != nil {
						return ctxErr
					}
					return memory.ErrUnavailable
				}
			} else if err := waitRetry(ctx, store.poisoned, delay); err != nil {
				return err
			}
			continue
		}

		if _, err := conn.ExecContext(context.Background(), "PRAGMA busy_timeout=0"); err != nil {
			if rollbackErr := store.rollback(conn); rollbackErr != nil {
				return rollbackErr
			}
			store.quarantine(conn)
			return memory.ErrUnavailable
		}

		callbackErr := callback(ctx, conn)
		if callbackErr != nil {
			if err := store.rollback(conn); err != nil {
				return err
			}
			if err := store.restoreAndReturnConnection(conn); err != nil {
				return err
			}
			return safeSQLiteError(ctx, callbackErr)
		}
		if hook := loadTestHooks().beforeCommitCheck; hook != nil {
			hook()
		}
		if err := ctx.Err(); err != nil {
			if rollbackErr := store.rollback(conn); rollbackErr != nil {
				return rollbackErr
			}
			if restoreErr := store.restoreAndReturnConnection(conn); restoreErr != nil {
				return restoreErr
			}
			return err
		}

		hooks = loadTestHooks()
		if hooks.commitStarted != nil {
			hooks.commitStarted()
		}
		commitErr := executeDriverControl(conn, "COMMIT")
		if commitErr != nil {
			store.quarantine(conn)
			return commitUnknown
		}
		// COMMIT success is the operation result. A later connection reset
		// failure still quarantines the Store, but must not turn durable success
		// into a retryable-looking failure.
		_ = store.restoreAndReturnConnection(conn)
		return nil
	}
}

func retryBackoff(attempt int, seed *uint64) time.Duration {
	if attempt > 5 {
		attempt = 5
	}
	base := time.Millisecond << attempt
	*seed ^= *seed << 13
	*seed ^= *seed >> 7
	*seed ^= *seed << 17
	jitter := time.Duration(*seed % uint64(base/2+1))
	return base/2 + jitter
}

func waitRetry(ctx context.Context, poisoned <-chan struct{}, delay time.Duration) error {
	if delay <= 0 {
		return nil
	}
	timer := time.NewTimer(delay)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-poisoned:
		return memory.ErrUnavailable
	case <-timer.C:
		return nil
	}
}

func (store *Store) borrowConnection(ctx context.Context) (*sql.Conn, error) {
	select {
	case <-ctx.Done():
		return nil, ctx.Err()
	case <-store.poisoned:
		return nil, memory.ErrUnavailable
	case conn := <-store.connections:
		select {
		case <-store.poisoned:
			// Poison rejects new work, but it does not make this healthy retained
			// connection ambiguous. Return it for exact owner accounting.
			store.returnConnection(conn)
			return nil, memory.ErrUnavailable
		default:
			if hook := loadTestHooks().afterConnectionAcquired; hook != nil {
				hook()
			}
			return conn, nil
		}
	}
}

func (store *Store) restoreAndReturnConnection(conn *sql.Conn) error {
	if conn == nil {
		return memory.ErrUnavailable
	}
	milliseconds, err := ceilMilliseconds(store.busyTimeout)
	if err != nil {
		store.quarantine(conn)
		return memory.ErrUnavailable
	}
	statement := "PRAGMA busy_timeout=" + strconv.FormatInt(milliseconds, 10)
	exec := func() error {
		_, err := conn.ExecContext(context.Background(), statement)
		return err
	}
	var resetErr error
	if hook := loadTestHooks().restoreBusyTimeout; hook != nil {
		resetErr = hook(statement, exec)
	} else {
		resetErr = exec()
	}
	if resetErr != nil {
		store.quarantine(conn)
		return memory.ErrUnavailable
	}
	store.returnConnection(conn)
	return nil
}

func (store *Store) returnConnection(conn *sql.Conn) {
	if conn == nil {
		return
	}
	// Only the connection with an uncertain driver COMMIT (or another
	// independently failed connection) is quarantined. Poison is Store-level
	// admission state, not evidence that healthy borrowed connections failed.
	store.connections <- conn
}

func (store *Store) rollback(conn *sql.Conn) error {
	if err := executeDriverControl(conn, "ROLLBACK"); err != nil {
		store.quarantine(conn)
		return memory.ErrUnavailable
	}
	return nil
}

func executeDriverControl(conn *sql.Conn, statement string) error {
	exec := func() error {
		_, err := conn.ExecContext(context.Background(), statement)
		return err
	}
	if hook := loadTestHooks().driverExec; hook != nil {
		return hook(statement, exec)
	}
	return exec()
}

func (store *Store) quarantine(conn *sql.Conn) {
	store.poison()
	if conn == nil {
		return
	}
	store.quarantined.Add(1)
	_ = conn.Raw(func(any) error { return driver.ErrBadConn })
	_ = conn.Close()
}
