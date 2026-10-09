package main

import (
	"math"
	"sort"
	"testing"
)

// Production-review regressions retained after the corresponding fixes.

// GetDegree("NaN") previously parsed successfully and returned NaN with a nil
// error: strconv.ParseFloat accepts it, and `NaN < -180 || NaN > 180` is
// false for NaN, so the range check passes. A NaN coordinate must never be
// forwarded to the vehicle.
func TestReviewGetDegreeRejectsNaN(t *testing.T) {
	v, err := GetDegree("NaN")
	if err == nil {
		t.Fatalf("GetDegree(NaN) = %v, nil error; NaN must be rejected (got v=%v IsNaN=%v)", v, v, math.IsNaN(float64(v)))
	}
}

// Map iteration previously made `state --help` list categories in random
// order on every invocation. Help output must remain stable (sorted).
func TestReviewCategoryNamesAreSorted(t *testing.T) {
	names := categoryNames()
	if !sort.StringsAreSorted(names) {
		sorted := append([]string(nil), names...)
		sort.Strings(sorted)
		t.Fatalf("categoryNames() unsorted: got %q, want %q", names, sorted)
	}
}
