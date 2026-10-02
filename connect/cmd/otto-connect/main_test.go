package main

import (
	"reflect"
	"testing"
)

func TestWithoutVarsDropsTokenVariable(t *testing.T) {
	env := []string{"PATH=/bin", "TG_TOKEN=123:abc", "TG_TOKEN_X=keep", "HOME=/h"}
	got := withoutVars(env, "TG_TOKEN")
	want := []string{"PATH=/bin", "TG_TOKEN_X=keep", "HOME=/h"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}
}
