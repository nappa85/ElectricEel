package main

import (
	"math"
	"sort"
	"testing"
)

// Production-review regressions. Each test asserts correct behavior and
// fails against the live code until the fix lands.

// GetDegree("NaN") currently parses successfully and returns NaN with a nil
// error: strconv.ParseFloat accepts it, and `NaN < -180 || NaN > 180` is
// false for NaN, so the range check passes. A NaN coordinate must never be
// forwarded to the vehicle.
func TestReviewGetDegreeRejectsNaN(t *testing.T) {
	v, err := GetDegree("NaN")
	if err == nil {
		t.Fatalf("GetDegree(NaN) = %v, nil error; NaN must be rejected (got v=%v IsNaN=%v)", v, v, math.IsNaN(float64(v)))
	}
}

// categoryNames() iterates a Go map, so `state --help` lists categories in a
// random order on every invocation. Help output must be stable (sorted).
func TestReviewCategoryNamesAreSorted(t *testing.T) {
	names := categoryNames()
	if !sort.StringsAreSorted(names) {
		sorted := append([]string(nil), names...)
		sort.Strings(sorted)
		t.Fatalf("categoryNames() unsorted: got %q, want %q (fails until names are sorted)", names, sorted)
	}
}
