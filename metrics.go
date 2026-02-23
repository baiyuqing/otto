package main

import (
	"math"
	"sort"
	"sync/atomic"
	"time"
)

type histogram struct {
	bounds []float64
	counts []atomic.Uint64
	sumMS  atomic.Uint64
	count  atomic.Uint64
	maxMS  atomic.Uint64
}

func newHistogram(bounds []float64) *histogram {
	h := &histogram{bounds: append([]float64(nil), bounds...), counts: make([]atomic.Uint64, len(bounds)+1)}
	return h
}

func (h *histogram) observe(ms float64) {
	h.count.Add(1)
	h.sumMS.Add(uint64(ms * 1000))
	for {
		old := h.maxMS.Load()
		newMax := uint64(ms * 1000)
		if newMax <= old || h.maxMS.CompareAndSwap(old, newMax) {
			break
		}
	}
	idx := sort.SearchFloat64s(h.bounds, ms)
	h.counts[idx].Add(1)
}

func (h *histogram) snapshot() histogramSnapshot {
	hs := histogramSnapshot{bucketCounts: make([]uint64, len(h.counts)), bounds: append([]float64(nil), h.bounds...)}
	hs.count = h.count.Load()
	hs.sumMS = float64(h.sumMS.Load()) / 1000
	hs.maxMS = float64(h.maxMS.Load()) / 1000
	for i := range h.counts {
		hs.bucketCounts[i] = h.counts[i].Load()
	}
	return hs
}

func (h *histogram) snapshotAndReset() histogramSnapshot {
	hs := histogramSnapshot{bucketCounts: make([]uint64, len(h.counts)), bounds: append([]float64(nil), h.bounds...)}
	hs.count = h.count.Swap(0)
	hs.sumMS = float64(h.sumMS.Swap(0)) / 1000
	hs.maxMS = float64(h.maxMS.Swap(0)) / 1000
	for i := range h.counts {
		hs.bucketCounts[i] = h.counts[i].Swap(0)
	}
	return hs
}

type histogramSnapshot struct {
	bounds       []float64
	bucketCounts []uint64
	sumMS        float64
	count        uint64
	maxMS        float64
}

func (h histogramSnapshot) quantile(q float64) float64 {
	if h.count == 0 {
		return 0
	}
	target := uint64(math.Ceil(float64(h.count) * q))
	if target == 0 {
		target = 1
	}
	var cumulative uint64
	for i, c := range h.bucketCounts {
		cumulative += c
		if cumulative >= target {
			if i < len(h.bounds) {
				return h.bounds[i]
			}
			return h.maxMS
		}
	}
	return h.maxMS
}

type metrics struct {
	totalSuccess atomic.Uint64
	totalFailure atomic.Uint64
	windowErrors atomic.Uint64

	totalHistogram  *histogram
	windowHistogram *histogram
	startedAt       time.Time
}

func newMetrics(start time.Time) *metrics {
	// Upper bounds in milliseconds. Keep this list compact to minimize memory/CPU overhead.
	bounds := []float64{0.25, 0.5, 1, 2, 5, 10, 20, 50, 100, 250, 500, 1000, 2000, 5000, 10000}
	return &metrics{
		totalHistogram:  newHistogram(bounds),
		windowHistogram: newHistogram(bounds),
		startedAt:       start,
	}
}

func (m *metrics) record(duration time.Duration, err error) {
	if err != nil {
		m.totalFailure.Add(1)
		m.windowErrors.Add(1)
		return
	}

	ms := float64(duration.Microseconds()) / 1000.0
	m.totalSuccess.Add(1)
	m.totalHistogram.observe(ms)
	m.windowHistogram.observe(ms)
}

func (m *metrics) snapshotWindow() (errs uint64, hs histogramSnapshot) {
	errs = m.windowErrors.Swap(0)
	hs = m.windowHistogram.snapshotAndReset()
	return
}

func (m *metrics) snapshotTotal() (success, failures uint64, hs histogramSnapshot) {
	success = m.totalSuccess.Load()
	failures = m.totalFailure.Load()
	hs = m.totalHistogram.snapshot()
	return
}

func quantile(sorted []float64, q float64) float64 {
	if len(sorted) == 0 {
		return 0
	}
	if q <= 0 {
		return sorted[0]
	}
	if q >= 1 {
		return sorted[len(sorted)-1]
	}
	idx := int(math.Ceil(q*float64(len(sorted)))) - 1
	if idx < 0 {
		idx = 0
	}
	if idx >= len(sorted) {
		idx = len(sorted) - 1
	}
	return sorted[idx]
}
