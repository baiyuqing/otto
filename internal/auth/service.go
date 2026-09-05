package auth

import (
	"context"
	"errors"
	"os"
)

var (
	ErrLoginFailed        = errors.New("chatgpt sign-in failed")
	ErrCredentialsRemoval = errors.New("stored chatgpt credentials could not be removed")
)

// Service owns the configured credential file and the operations that mutate
// or inspect it. Frontends receive only the narrow app capability.
type Service struct {
	path  string
	login func(context.Context, func(string) error) (Credentials, error)
}

func NewService(path string, login ...func(context.Context, func(string) error) (Credentials, error)) *Service {
	flow := Login
	if len(login) > 0 && login[0] != nil {
		flow = login[0]
	}
	return &Service{path: path, login: flow}
}

func (s *Service) Login(ctx context.Context, open func(string) error) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	if open == nil {
		return ErrLoginFailed
	}
	creds, err := s.login(ctx, open)
	if err != nil {
		if ctxErr := ctx.Err(); ctxErr != nil {
			return ctxErr
		}
		return ErrLoginFailed
	}
	if err := ctx.Err(); err != nil {
		return err
	}
	if err := creds.Save(s.path); err != nil {
		return ErrCredentialsPersistence
	}
	return nil
}

func (s *Service) Logout(ctx context.Context) (bool, error) {
	if err := ctx.Err(); err != nil {
		return false, err
	}
	if err := os.Remove(s.path); err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return false, nil
		}
		return false, ErrCredentialsRemoval
	}
	return true, nil
}

func (s *Service) Status(ctx context.Context) (string, bool) {
	if ctx.Err() != nil {
		return ErrInteractiveUnavailable.Error(), false
	}
	return StatusLine(s.path)
}
