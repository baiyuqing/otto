package main

import (
	"context"
	"database/sql"
	"strings"
	"time"

	_ "github.com/go-sql-driver/mysql"
)

func runQuery(ctx context.Context, db *sql.DB, query string) error {
	tx, err := db.BeginTx(ctx, nil)
	if err != nil {
		return err
	}

	rows, err := tx.QueryContext(ctx, query)
	if err != nil {
		if strings.Contains(strings.ToLower(err.Error()), "does not return rows") {
			_, execErr := tx.ExecContext(ctx, query)
			if execErr != nil {
				_ = tx.Rollback()
				return execErr
			}
			return tx.Commit()
		}
		_ = tx.Rollback()
		return err
	}
	defer rows.Close()

	cols, err := rows.Columns()
	if err != nil {
		_ = tx.Rollback()
		return err
	}
	vals := make([]any, len(cols))
	scanArgs := make([]any, len(cols))
	for i := range vals {
		scanArgs[i] = &vals[i]
	}
	for rows.Next() {
		if err := rows.Scan(scanArgs...); err != nil {
			_ = tx.Rollback()
			return err
		}
	}
	if err := rows.Err(); err != nil {
		_ = tx.Rollback()
		return err
	}
	return tx.Commit()
}

type queryRunner func(context.Context) error

func makeQueryRunner(cfg config) (queryRunner, func(), error) {
	if cfg.connectionMode == connectionModePerTxn {
		runner := func(ctx context.Context) error {
			db, err := sql.Open("mysql", cfg.dsn)
			if err != nil {
				return err
			}
			db.SetMaxOpenConns(1)
			db.SetMaxIdleConns(0)
			defer db.Close()
			return runQuery(ctx, db, cfg.query)
		}
		return runner, func() {}, nil
	}

	db, err := sql.Open("mysql", cfg.dsn)
	if err != nil {
		return nil, nil, err
	}
	db.SetMaxOpenConns(cfg.concurrency)
	db.SetMaxIdleConns(cfg.concurrency)

	runner := func(ctx context.Context) error {
		return runQuery(ctx, db, cfg.query)
	}
	cleanup := func() { _ = db.Close() }
	return runner, cleanup, nil
}

func worker(ctx context.Context, run queryRunner, m *metrics) {
	for {
		select {
		case <-ctx.Done():
			return
		default:
		}
		start := time.Now()
		err := run(ctx)
		m.record(time.Since(start), err)
	}
}
