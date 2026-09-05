package app

import (
	"context"
	"errors"
	"testing"
)

type profileSwitcherStub struct {
	switchErr error
	setErr    error
	switched  bool
	set       bool
}

func (s *profileSwitcherStub) Profiles() []string { return nil }
func (s *profileSwitcherStub) SwitchProfile(context.Context, string) (ResumeResult, error) {
	s.switched = true
	return ResumeResult{SessionPath: "next"}, s.switchErr
}
func (s *profileSwitcherStub) SetDefaultProfile(context.Context, string) error {
	s.set = true
	return s.setErr
}

func TestSelectProfileStopsAfterSwitchFailure(t *testing.T) {
	switchErr := errors.New("switch failed")
	switcher := &profileSwitcherStub{switchErr: switchErr}
	result, err := SelectProfile(context.Background(), switcher, "chatgpt")
	if !errors.Is(err, switchErr) || result.Switched || switcher.set {
		t.Fatalf("result=%+v err=%v switched=%v set=%v", result, err, switcher.switched, switcher.set)
	}
}

func TestSelectProfileReportsDefaultPersistenceAfterSwitch(t *testing.T) {
	setErr := errors.New("default save failed")
	switcher := &profileSwitcherStub{setErr: setErr}
	result, err := SelectProfile(context.Background(), switcher, "chatgpt")
	if !errors.Is(err, setErr) || !result.Switched || result.SessionPath != "next" || !switcher.set {
		t.Fatalf("result=%+v err=%v switched=%v set=%v", result, err, result.Switched, switcher.set)
	}
}

func TestSelectProfileSuccess(t *testing.T) {
	switcher := &profileSwitcherStub{}
	result, err := SelectProfile(context.Background(), switcher, "chatgpt")
	if err != nil || !result.Switched || result.SessionPath != "next" {
		t.Fatalf("result=%+v err=%v", result, err)
	}
}
