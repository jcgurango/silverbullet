package server

import (
	"fmt"
	"strings"
)

// ThreeWayMerge performs a line-based 3-way merge.
// base is the common ancestor, ours is the server's version, theirs is the client's version.
// Returns the merged content, whether there were conflicts, and any error.
func ThreeWayMerge(base, ours, theirs []byte) ([]byte, bool, error) {
	baseLines := splitLines(string(base))
	ourLines := splitLines(string(ours))
	theirLines := splitLines(string(theirs))

	// Compute diffs from base to each side
	ourDiff := diffLines(baseLines, ourLines)
	theirDiff := diffLines(baseLines, theirLines)

	// Convert diffs to edit hunks
	ourHunks := toHunks(ourDiff)
	theirHunks := toHunks(theirDiff)

	// Merge the hunks
	merged, hasConflict := mergeHunks(baseLines, ourHunks, theirHunks)

	return []byte(strings.Join(merged, "\n")), hasConflict, nil
}

// splitLines splits text into lines. An empty string produces a single empty line.
func splitLines(text string) []string {
	if text == "" {
		return []string{}
	}
	return strings.Split(text, "\n")
}

// diffOp represents a diff operation
type diffOp int

const (
	diffEqual  diffOp = iota
	diffInsert        // lines added in the new version
	diffDelete        // lines removed from the old version
)

// diffEntry represents one entry in a diff
type diffEntry struct {
	op    diffOp
	lines []string
}

// hunk represents a contiguous region of changes
type hunk struct {
	baseStart int      // start position in base
	baseLen   int      // number of base lines replaced
	newLines  []string // replacement lines
}

// diffLines computes a line-level diff using the LCS algorithm.
func diffLines(a, b []string) []diffEntry {
	// Compute LCS table
	m, n := len(a), len(b)
	dp := make([][]int, m+1)
	for i := range dp {
		dp[i] = make([]int, n+1)
	}
	for i := 1; i <= m; i++ {
		for j := 1; j <= n; j++ {
			if a[i-1] == b[j-1] {
				dp[i][j] = dp[i-1][j-1] + 1
			} else if dp[i-1][j] >= dp[i][j-1] {
				dp[i][j] = dp[i-1][j]
			} else {
				dp[i][j] = dp[i][j-1]
			}
		}
	}

	// Backtrack to build diff
	var result []diffEntry
	i, j := m, n
	var stack []diffEntry

	for i > 0 || j > 0 {
		if i > 0 && j > 0 && a[i-1] == b[j-1] {
			stack = append(stack, diffEntry{op: diffEqual, lines: []string{a[i-1]}})
			i--
			j--
		} else if j > 0 && (i == 0 || dp[i][j-1] >= dp[i-1][j]) {
			stack = append(stack, diffEntry{op: diffInsert, lines: []string{b[j-1]}})
			j--
		} else {
			stack = append(stack, diffEntry{op: diffDelete, lines: []string{a[i-1]}})
			i--
		}
	}

	// Reverse the stack
	for k := len(stack) - 1; k >= 0; k-- {
		result = append(result, stack[k])
	}

	// Merge consecutive entries of the same type
	return compactDiff(result)
}

// compactDiff merges consecutive diff entries of the same type.
func compactDiff(entries []diffEntry) []diffEntry {
	if len(entries) == 0 {
		return entries
	}

	var result []diffEntry
	current := entries[0]

	for i := 1; i < len(entries); i++ {
		if entries[i].op == current.op {
			current.lines = append(current.lines, entries[i].lines...)
		} else {
			result = append(result, current)
			current = entries[i]
		}
	}
	result = append(result, current)
	return result
}

// toHunks converts a diff into a list of hunks (change regions).
func toHunks(diff []diffEntry) []hunk {
	var hunks []hunk
	basePos := 0

	for i := 0; i < len(diff); i++ {
		entry := diff[i]
		switch entry.op {
		case diffEqual:
			basePos += len(entry.lines)
		case diffDelete:
			// Check if followed by an insert (replacement)
			h := hunk{
				baseStart: basePos,
				baseLen:   len(entry.lines),
			}
			if i+1 < len(diff) && diff[i+1].op == diffInsert {
				h.newLines = diff[i+1].lines
				i++ // skip the insert
			}
			hunks = append(hunks, h)
			basePos += h.baseLen
		case diffInsert:
			// Pure insertion (no preceding delete)
			hunks = append(hunks, hunk{
				baseStart: basePos,
				baseLen:   0,
				newLines:  entry.lines,
			})
		}
	}

	return hunks
}

// mergeHunks merges two sets of hunks against a common base.
// Returns the merged lines and whether any conflicts were found.
func mergeHunks(base []string, ourHunks, theirHunks []hunk) ([]string, bool) {
	hasConflict := false
	var result []string

	oi, ti := 0, 0 // indices into our/their hunks
	basePos := 0

	for basePos <= len(base) || oi < len(ourHunks) || ti < len(theirHunks) {
		// Find the next hunk from either side
		var nextOur, nextTheir *hunk
		if oi < len(ourHunks) {
			nextOur = &ourHunks[oi]
		}
		if ti < len(theirHunks) {
			nextTheir = &theirHunks[ti]
		}

		// No more hunks — copy remaining base
		if nextOur == nil && nextTheir == nil {
			if basePos < len(base) {
				result = append(result, base[basePos:]...)
			}
			break
		}

		// Determine which hunk comes first
		if nextOur != nil && nextTheir != nil && hunksOverlap(*nextOur, *nextTheir) {
			// Overlapping hunks — potential conflict
			// First, copy base lines up to the start of the earlier hunk
			start := min(nextOur.baseStart, nextTheir.baseStart)
			if basePos < start {
				result = append(result, base[basePos:start]...)
				basePos = start
			}

			// Determine the combined range
			end := max(nextOur.baseStart+nextOur.baseLen, nextTheir.baseStart+nextTheir.baseLen)

			// Check if both sides made the same change
			if sameChange(*nextOur, *nextTheir, base) {
				// Same change on both sides — no conflict
				result = append(result, nextOur.newLines...)
			} else {
				// Conflict!
				hasConflict = true
				result = append(result, "<<<<<<< server")
				// Our version of this region
				result = append(result, applyHunkToRegion(base, start, end, *nextOur)...)
				result = append(result, "=======")
				// Their version of this region
				result = append(result, applyHunkToRegion(base, start, end, *nextTheir)...)
				result = append(result, ">>>>>>> client")
			}

			basePos = end
			oi++
			ti++
			continue
		}

		// Non-overlapping: apply whichever comes first
		var nextHunk *hunk
		isOurs := false
		if nextOur != nil && (nextTheir == nil || nextOur.baseStart <= nextTheir.baseStart) {
			nextHunk = nextOur
			isOurs = true
		} else {
			nextHunk = nextTheir
		}

		// Copy base lines up to the hunk
		if basePos < nextHunk.baseStart {
			result = append(result, base[basePos:nextHunk.baseStart]...)
			basePos = nextHunk.baseStart
		}

		// Apply the hunk
		result = append(result, nextHunk.newLines...)
		basePos = nextHunk.baseStart + nextHunk.baseLen

		if isOurs {
			oi++
		} else {
			ti++
		}
	}

	return result, hasConflict
}

// hunksOverlap checks if two hunks affect overlapping regions of the base.
func hunksOverlap(a, b hunk) bool {
	aEnd := a.baseStart + a.baseLen
	bEnd := b.baseStart + b.baseLen

	// For insertions at the same position, they overlap
	if a.baseLen == 0 && b.baseLen == 0 && a.baseStart == b.baseStart {
		return true
	}

	// Handle insertion at a boundary of a delete/replace
	if a.baseLen == 0 {
		return a.baseStart > b.baseStart && a.baseStart < bEnd
	}
	if b.baseLen == 0 {
		return b.baseStart > a.baseStart && b.baseStart < aEnd
	}

	return a.baseStart < bEnd && b.baseStart < aEnd
}

// sameChange checks if two hunks produce the same result.
func sameChange(a, b hunk, base []string) bool {
	if a.baseStart != b.baseStart || a.baseLen != b.baseLen {
		return false
	}
	if len(a.newLines) != len(b.newLines) {
		return false
	}
	for i := range a.newLines {
		if a.newLines[i] != b.newLines[i] {
			return false
		}
	}
	return true
}

// applyHunkToRegion reconstructs what a region of the base looks like
// after applying a single hunk. The region is [start, end) in the base.
func applyHunkToRegion(base []string, start, end int, h hunk) []string {
	var result []string

	// Lines before the hunk within the region
	if start < h.baseStart {
		result = append(result, base[start:h.baseStart]...)
	}

	// The hunk's replacement
	result = append(result, h.newLines...)

	// Lines after the hunk within the region
	hunkEnd := h.baseStart + h.baseLen
	if hunkEnd < end {
		result = append(result, base[hunkEnd:end]...)
	}

	return result
}

// min returns the smaller of two ints.
func minInt(a, b int) int {
	if a < b {
		return a
	}
	return b
}

// max returns the larger of two ints.
func maxInt(a, b int) int {
	if a > b {
		return a
	}
	return b
}

// ParseConflictMarkers extracts the two sides from a document containing
// git-style conflict markers. Returns (ours, theirs, hasConflicts).
// If there are no conflict markers, returns the original text for both sides.
func ParseConflictMarkers(text string) (ours string, theirs string, hasConflicts bool) {
	lines := strings.Split(text, "\n")
	var oursLines, theirsLines []string
	inConflict := false
	inOurs := false

	for _, line := range lines {
		if strings.HasPrefix(line, "<<<<<<< ") {
			hasConflicts = true
			inConflict = true
			inOurs = true
			continue
		}
		if inConflict && line == "=======" {
			inOurs = false
			continue
		}
		if strings.HasPrefix(line, ">>>>>>> ") {
			inConflict = false
			continue
		}

		if inConflict {
			if inOurs {
				oursLines = append(oursLines, line)
			} else {
				theirsLines = append(theirsLines, line)
			}
		} else {
			oursLines = append(oursLines, line)
			theirsLines = append(theirsLines, line)
		}
	}

	if !hasConflicts {
		return text, text, false
	}

	return strings.Join(oursLines, "\n"), strings.Join(theirsLines, "\n"), true
}

// HasConflictMarkers checks whether the given text contains conflict markers.
func HasConflictMarkers(text string) bool {
	return strings.Contains(text, fmt.Sprintf("\n<<<<<<< ")) ||
		strings.HasPrefix(text, "<<<<<<< ")
}
