package auth

import (
	"context"
	"strings"
)

type contextKey string

const authPathContextKey contextKey = "chatgpt-auth-path"

func ContextWithPath(ctx context.Context, path string) context.Context {
	if ctx == nil {
		ctx = context.Background()
	}
	return context.WithValue(ctx, authPathContextKey, strings.Clone(path))
}

func PathFromContext(ctx context.Context) (string, bool) {
	if ctx == nil {
		return "", false
	}
	path, ok := ctx.Value(authPathContextKey).(string)
	return path, ok && path != ""
}
