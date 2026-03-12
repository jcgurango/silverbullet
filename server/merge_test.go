package server

import (
	"testing"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func TestThreeWayMerge_NoChanges(t *testing.T) {
	base := []byte("line1\nline2\nline3")
	merged, hasConflict, err := ThreeWayMerge(base, base, base)
	require.NoError(t, err)
	assert.False(t, hasConflict)
	assert.Equal(t, string(base), string(merged))
}

func TestThreeWayMerge_OnlyOursChanged(t *testing.T) {
	base := []byte("line1\nline2\nline3")
	ours := []byte("line1\nmodified\nline3")
	merged, hasConflict, err := ThreeWayMerge(base, ours, base)
	require.NoError(t, err)
	assert.False(t, hasConflict)
	assert.Equal(t, string(ours), string(merged))
}

func TestThreeWayMerge_OnlyTheirsChanged(t *testing.T) {
	base := []byte("line1\nline2\nline3")
	theirs := []byte("line1\nline2\nmodified")
	merged, hasConflict, err := ThreeWayMerge(base, base, theirs)
	require.NoError(t, err)
	assert.False(t, hasConflict)
	assert.Equal(t, string(theirs), string(merged))
}

func TestThreeWayMerge_BothChangedDifferentLines(t *testing.T) {
	base := []byte("line1\nline2\nline3\nline4")
	ours := []byte("modified1\nline2\nline3\nline4")
	theirs := []byte("line1\nline2\nline3\nmodified4")
	merged, hasConflict, err := ThreeWayMerge(base, ours, theirs)
	require.NoError(t, err)
	assert.False(t, hasConflict)
	assert.Equal(t, "modified1\nline2\nline3\nmodified4", string(merged))
}

func TestThreeWayMerge_Conflict(t *testing.T) {
	base := []byte("line1\nline2\nline3")
	ours := []byte("line1\nours\nline3")
	theirs := []byte("line1\ntheirs\nline3")
	merged, hasConflict, err := ThreeWayMerge(base, ours, theirs)
	require.NoError(t, err)
	assert.True(t, hasConflict)
	result := string(merged)
	assert.Contains(t, result, "<<<<<<< server")
	assert.Contains(t, result, "ours")
	assert.Contains(t, result, "=======")
	assert.Contains(t, result, "theirs")
	assert.Contains(t, result, ">>>>>>> client")
}

func TestThreeWayMerge_SameChangeOnBothSides(t *testing.T) {
	base := []byte("line1\nline2\nline3")
	both := []byte("line1\nsame change\nline3")
	merged, hasConflict, err := ThreeWayMerge(base, both, both)
	require.NoError(t, err)
	assert.False(t, hasConflict)
	assert.Equal(t, string(both), string(merged))
}

func TestThreeWayMerge_OursAddsLines(t *testing.T) {
	base := []byte("line1\nline3")
	ours := []byte("line1\nline2\nline3")
	merged, hasConflict, err := ThreeWayMerge(base, ours, base)
	require.NoError(t, err)
	assert.False(t, hasConflict)
	assert.Equal(t, string(ours), string(merged))
}

func TestThreeWayMerge_TheirsDeletesLines(t *testing.T) {
	base := []byte("line1\nline2\nline3")
	theirs := []byte("line1\nline3")
	merged, hasConflict, err := ThreeWayMerge(base, base, theirs)
	require.NoError(t, err)
	assert.False(t, hasConflict)
	assert.Equal(t, string(theirs), string(merged))
}

func TestThreeWayMerge_BothAddAtSamePlace_Conflict(t *testing.T) {
	base := []byte("line1\nline3")
	ours := []byte("line1\nour insert\nline3")
	theirs := []byte("line1\ntheir insert\nline3")
	merged, hasConflict, err := ThreeWayMerge(base, ours, theirs)
	require.NoError(t, err)
	assert.True(t, hasConflict)
	result := string(merged)
	assert.Contains(t, result, "<<<<<<< server")
	assert.Contains(t, result, "our insert")
	assert.Contains(t, result, "their insert")
	assert.Contains(t, result, ">>>>>>> client")
}

func TestThreeWayMerge_EmptyBase(t *testing.T) {
	base := []byte("")
	ours := []byte("new content from server")
	theirs := []byte("new content from client")
	merged, hasConflict, err := ThreeWayMerge(base, ours, theirs)
	require.NoError(t, err)
	assert.True(t, hasConflict)
	result := string(merged)
	assert.Contains(t, result, "<<<<<<< server")
	assert.Contains(t, result, ">>>>>>> client")
}

func TestThreeWayMerge_RealWorldMarkdown(t *testing.T) {
	base := []byte(`# My Page

Some introductory text.

## Section A

Content of section A.

## Section B

Content of section B.`)

	ours := []byte(`# My Page

Some introductory text.

## Section A

Updated content of section A by server.

## Section B

Content of section B.`)

	theirs := []byte(`# My Page

Some introductory text.

## Section A

Content of section A.

## Section B

Updated content of section B by client.`)

	merged, hasConflict, err := ThreeWayMerge(base, ours, theirs)
	require.NoError(t, err)
	assert.False(t, hasConflict)
	result := string(merged)
	assert.Contains(t, result, "Updated content of section A by server.")
	assert.Contains(t, result, "Updated content of section B by client.")
}

func TestSplitLines(t *testing.T) {
	assert.Equal(t, []string{}, splitLines(""))
	assert.Equal(t, []string{"hello"}, splitLines("hello"))
	assert.Equal(t, []string{"a", "b"}, splitLines("a\nb"))
	assert.Equal(t, []string{"a", ""}, splitLines("a\n"))
}

func TestParseConflictMarkers(t *testing.T) {
	text := `line1
<<<<<<< server
server line
=======
client line
>>>>>>> client
line3`

	ours, theirs, hasConflicts := ParseConflictMarkers(text)
	assert.True(t, hasConflicts)
	assert.Equal(t, "line1\nserver line\nline3", ours)
	assert.Equal(t, "line1\nclient line\nline3", theirs)
}

func TestParseConflictMarkers_NoConflict(t *testing.T) {
	text := "just normal text\nno conflicts here"
	ours, theirs, hasConflicts := ParseConflictMarkers(text)
	assert.False(t, hasConflicts)
	assert.Equal(t, text, ours)
	assert.Equal(t, text, theirs)
}

func TestHasConflictMarkers(t *testing.T) {
	assert.True(t, HasConflictMarkers("<<<<<<< server\nfoo\n=======\nbar\n>>>>>>> client"))
	assert.True(t, HasConflictMarkers("line1\n<<<<<<< server\nfoo"))
	assert.False(t, HasConflictMarkers("normal text"))
	assert.False(t, HasConflictMarkers(""))
}

func TestDiffLines(t *testing.T) {
	a := []string{"a", "b", "c"}
	b := []string{"a", "x", "c"}
	diff := diffLines(a, b)

	// Should have: equal(a), delete(b)+insert(x), equal(c)
	found := false
	for _, d := range diff {
		if d.op == diffInsert {
			assert.Equal(t, []string{"x"}, d.lines)
			found = true
		}
	}
	assert.True(t, found, "should find an insert of 'x'")
}
