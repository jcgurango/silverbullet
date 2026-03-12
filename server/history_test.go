package server

import (
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

func setupTestHistory(t *testing.T) (*HistoryStore, string) {
	dir := t.TempDir()
	h := NewHistoryStore(dir, time.Minute)
	return h, dir
}

func TestSha256Hex(t *testing.T) {
	hash := sha256Hex([]byte("hello world"))
	assert.Equal(t, "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9", hash)
}

func TestEnsureInitialized(t *testing.T) {
	h, _ := setupTestHistory(t)
	content := []byte("# Hello\n\nThis is a test page.")

	err := h.EnsureInitialized("test.md", content)
	require.NoError(t, err)

	// HEAD should be set
	head, err := h.GetHead("test.md")
	require.NoError(t, err)
	assert.Equal(t, sha256Hex(content), head)

	// Content should be retrievable
	stored, err := h.GetContent("test.md", head)
	require.NoError(t, err)
	assert.Equal(t, content, stored)

	// Commits should have one entry
	commits, err := h.GetCommits("test.md")
	require.NoError(t, err)
	assert.Len(t, commits, 1)
	assert.Equal(t, head, commits[0].Hash)
	assert.Equal(t, "", commits[0].Parent)
	assert.Equal(t, "disk", commits[0].Source)

	// Calling again should be a no-op
	err = h.EnsureInitialized("test.md", []byte("different content"))
	require.NoError(t, err)
	head2, _ := h.GetHead("test.md")
	assert.Equal(t, head, head2, "should not change head on re-init")
}

func TestRecordCommit_FastForward(t *testing.T) {
	h, _ := setupTestHistory(t)
	initial := []byte("initial content")

	err := h.EnsureInitialized("page.md", initial)
	require.NoError(t, err)
	initialHash, _ := h.GetHead("page.md")

	// Wait to avoid throttling
	h.minCommitInterval = 0

	updated := []byte("updated content")
	commit, throttled, err := h.RecordCommit("page.md", updated, initialHash, "client")
	require.NoError(t, err)
	assert.False(t, throttled)
	assert.Equal(t, sha256Hex(updated), commit.Hash)
	assert.Equal(t, initialHash, commit.Parent)

	// HEAD should be updated
	head, _ := h.GetHead("page.md")
	assert.Equal(t, commit.Hash, head)

	// Should have 2 commits
	commits, _ := h.GetCommits("page.md")
	assert.Len(t, commits, 2)
}

func TestRecordCommit_HashDedup(t *testing.T) {
	h, _ := setupTestHistory(t)
	h.minCommitInterval = 0
	content := []byte("same content")

	err := h.EnsureInitialized("page.md", content)
	require.NoError(t, err)

	// Writing the same content should be a no-op
	head, _ := h.GetHead("page.md")
	commit, throttled, err := h.RecordCommit("page.md", content, head, "client")
	require.NoError(t, err)
	assert.False(t, throttled)
	assert.Equal(t, head, commit.Hash)

	// Should still have just 1 commit
	commits, _ := h.GetCommits("page.md")
	assert.Len(t, commits, 1)
}

func TestRecordCommit_Throttling(t *testing.T) {
	h, _ := setupTestHistory(t)
	h.minCommitInterval = time.Hour // long interval to guarantee throttling

	err := h.EnsureInitialized("page.md", []byte("initial"))
	require.NoError(t, err)
	head, _ := h.GetHead("page.md")

	// Client write within throttle interval
	_, throttled, err := h.RecordCommit("page.md", []byte("updated"), head, "client")
	require.NoError(t, err)
	assert.True(t, throttled)

	// HEAD should NOT be updated (throttled)
	head2, _ := h.GetHead("page.md")
	assert.Equal(t, head, head2)

	// Disk writes should NOT be throttled
	_, throttled, err = h.RecordCommit("page.md", []byte("disk edit"), head, "disk")
	require.NoError(t, err)
	assert.False(t, throttled)

	head3, _ := h.GetHead("page.md")
	assert.Equal(t, sha256Hex([]byte("disk edit")), head3)
}

func TestFindLCA(t *testing.T) {
	h, _ := setupTestHistory(t)
	h.minCommitInterval = 0

	// Build a chain: A -> B -> C
	err := h.EnsureInitialized("page.md", []byte("A"))
	require.NoError(t, err)
	hashA, _ := h.GetHead("page.md")

	_, _, err = h.RecordCommit("page.md", []byte("B"), hashA, "client")
	require.NoError(t, err)
	hashB, _ := h.GetHead("page.md")

	_, _, err = h.RecordCommit("page.md", []byte("C"), hashB, "client")
	require.NoError(t, err)
	hashC, _ := h.GetHead("page.md")

	// Now simulate a branch: D from B (as if another client had B as parent)
	_, _, err = h.RecordCommit("page.md", []byte("D"), hashB, "disk")
	require.NoError(t, err)
	hashD, _ := h.GetHead("page.md")

	// LCA of C and D should be B
	lca, err := h.FindLCA("page.md", hashC, hashD)
	require.NoError(t, err)
	assert.Equal(t, hashB, lca)

	// LCA of A and C should be A
	lca, err = h.FindLCA("page.md", hashA, hashC)
	require.NoError(t, err)
	assert.Equal(t, hashA, lca)
}

func TestReconcileDiskState(t *testing.T) {
	h, _ := setupTestHistory(t)
	h.minCommitInterval = 0

	// No history yet — should initialize
	err := h.ReconcileDiskState("page.md", []byte("initial"))
	require.NoError(t, err)

	head, _ := h.GetHead("page.md")
	assert.Equal(t, sha256Hex([]byte("initial")), head)

	// Same content — no new commit
	err = h.ReconcileDiskState("page.md", []byte("initial"))
	require.NoError(t, err)
	commits, _ := h.GetCommits("page.md")
	assert.Len(t, commits, 1)

	// Different content — new commit
	err = h.ReconcileDiskState("page.md", []byte("modified externally"))
	require.NoError(t, err)
	commits, _ = h.GetCommits("page.md")
	assert.Len(t, commits, 2)
	head, _ = h.GetHead("page.md")
	assert.Equal(t, sha256Hex([]byte("modified externally")), head)
}

func TestDeleteHistory(t *testing.T) {
	h, _ := setupTestHistory(t)

	err := h.EnsureInitialized("page.md", []byte("content"))
	require.NoError(t, err)

	err = h.DeleteHistory("page.md")
	require.NoError(t, err)

	head, err := h.GetHead("page.md")
	require.NoError(t, err)
	assert.Equal(t, "", head)
}

func TestNestedPaths(t *testing.T) {
	h, _ := setupTestHistory(t)

	err := h.EnsureInitialized("journal/2024/01/01.md", []byte("journal entry"))
	require.NoError(t, err)

	head, err := h.GetHead("journal/2024/01/01.md")
	require.NoError(t, err)
	assert.NotEmpty(t, head)

	content, err := h.GetContent("journal/2024/01/01.md", head)
	require.NoError(t, err)
	assert.Equal(t, []byte("journal entry"), content)

	// Verify directory structure
	dir := h.historyDir("journal/2024/01/01.md")
	assert.True(t, filepath.IsAbs(dir))
	_, err = os.Stat(dir)
	require.NoError(t, err)
}
