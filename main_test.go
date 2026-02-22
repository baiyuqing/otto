package main

import "testing"

func TestQuantile(t *testing.T) {
	vals := []float64{1, 2, 3, 4, 5}
	if got := quantile(vals, 0.95); got != 5 {
		t.Fatalf("q95 got %v", got)
	}
	if got := quantile(vals, 0.99); got != 5 {
		t.Fatalf("q99 got %v", got)
	}
	if got := quantile(vals, 0.5); got != 3 {
		t.Fatalf("q50 got %v", got)
	}
}
