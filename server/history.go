package server

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

// HistoryStore manages version history for markdown files.
// It stores full content snapshots keyed by SHA-256 hash,
// with a commit log tracking the chain of versions.
type HistoryStore struct {
	rootPath          string // the space root path
	minCommitInterval time.Duration
	mu                sync.RWMutex
}

// Commit represents a single version in the history chain.
type Commit struct {
	Hash      string `json:"hash"`
	Parent    string `json:"parent"`
	Timestamp int64  `json:"timestamp"`
	Source    string `json:"source"` // "client" or "disk"
}

// NewHistoryStore creates a new HistoryStore.
func NewHistoryStore(rootPath string, minCommitInterval time.Duration) *HistoryStore {
	return &HistoryStore{
		rootPath:          rootPath,
		minCommitInterval: minCommitInterval,
	}
}

// historyDir returns the .history directory path for a given file.
// e.g., for "journal/2024/01.md" returns "<root>/.history/journal/2024/01.md.versions"
func (h *HistoryStore) historyDir(path string) string {
	return filepath.Join(h.rootPath, ".history", path+".versions")
}

// sha256Hex computes the SHA-256 hash of data and returns it as a hex string.
func sha256Hex(data []byte) string {
	sum := sha256.Sum256(data)
	return hex.EncodeToString(sum[:])
}

// objectPath returns the filesystem path for a content object.
// Uses 2-char prefix fan-out: objects/ab/abcdef...
func (h *HistoryStore) objectPath(path string, hash string) string {
	dir := h.historyDir(path)
	if len(hash) < 2 {
		return filepath.Join(dir, "objects", hash)
	}
	return filepath.Join(dir, "objects", hash[:2], hash)
}

// GetHead returns the current HEAD hash for a file, or "" if no history exists.
func (h *HistoryStore) GetHead(path string) (string, error) {
	h.mu.RLock()
	defer h.mu.RUnlock()

	headFile := filepath.Join(h.historyDir(path), "head")
	data, err := os.ReadFile(headFile)
	if err != nil {
		if os.IsNotExist(err) {
			return "", nil
		}
		return "", fmt.Errorf("failed to read head: %w", err)
	}
	return strings.TrimSpace(string(data)), nil
}

// GetCommits reads the full commit log for a file.
func (h *HistoryStore) GetCommits(path string) ([]Commit, error) {
	h.mu.RLock()
	defer h.mu.RUnlock()

	return h.getCommitsUnlocked(path)
}

func (h *HistoryStore) getCommitsUnlocked(path string) ([]Commit, error) {
	logFile := filepath.Join(h.historyDir(path), "commits.jsonl")
	data, err := os.ReadFile(logFile)
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil
		}
		return nil, fmt.Errorf("failed to read commits: %w", err)
	}

	var commits []Commit
	lines := strings.Split(strings.TrimSpace(string(data)), "\n")
	for _, line := range lines {
		line = strings.TrimSpace(line)
		if line == "" {
			continue
		}
		var c Commit
		if err := json.Unmarshal([]byte(line), &c); err != nil {
			return nil, fmt.Errorf("failed to parse commit: %w", err)
		}
		commits = append(commits, c)
	}
	return commits, nil
}

// GetContent reads the content for a given hash.
func (h *HistoryStore) GetContent(path string, hash string) ([]byte, error) {
	h.mu.RLock()
	defer h.mu.RUnlock()

	return h.getContentUnlocked(path, hash)
}

func (h *HistoryStore) getContentUnlocked(path string, hash string) ([]byte, error) {
	objPath := h.objectPath(path, hash)
	data, err := os.ReadFile(objPath)
	if err != nil {
		if os.IsNotExist(err) {
			return nil, fmt.Errorf("object not found: %s", hash)
		}
		return nil, fmt.Errorf("failed to read object: %w", err)
	}
	return data, nil
}

// GetLastCommitTime returns the timestamp of the most recent commit, or 0 if no history.
func (h *HistoryStore) GetLastCommitTime(path string) (int64, error) {
	h.mu.RLock()
	defer h.mu.RUnlock()

	commits, err := h.getCommitsUnlocked(path)
	if err != nil {
		return 0, err
	}
	if len(commits) == 0 {
		return 0, nil
	}
	return commits[len(commits)-1].Timestamp, nil
}

// RecordCommit stores a new commit. Returns the commit, whether it was throttled, and any error.
// If the content hash matches HEAD, this is a no-op (dedup).
// If within minCommitInterval and source is "client" and it's a fast-forward, returns throttled=true.
func (h *HistoryStore) RecordCommit(path string, content []byte, parentHash string, source string) (Commit, bool, error) {
	h.mu.Lock()
	defer h.mu.Unlock()

	contentHash := sha256Hex(content)

	// Read current HEAD
	headFile := filepath.Join(h.historyDir(path), "head")
	headData, err := os.ReadFile(headFile)
	currentHead := ""
	if err == nil {
		currentHead = strings.TrimSpace(string(headData))
	}

	// Hash dedup: if content matches HEAD, no-op
	if contentHash == currentHead {
		return Commit{Hash: contentHash, Parent: parentHash, Timestamp: time.Now().UnixMilli(), Source: source}, false, nil
	}

	// Throttle check: only for client fast-forwards
	if source == "client" && parentHash == currentHead {
		commits, err := h.getCommitsUnlocked(path)
		if err == nil && len(commits) > 0 {
			lastTime := commits[len(commits)-1].Timestamp
			if time.Since(time.UnixMilli(lastTime)) < h.minCommitInterval {
				// Throttled — caller should still write the file to disk
				return Commit{Hash: contentHash, Parent: parentHash, Timestamp: time.Now().UnixMilli(), Source: source}, true, nil
			}
		}
	}

	// Store the content object
	objPath := h.objectPath(path, contentHash)
	if err := os.MkdirAll(filepath.Dir(objPath), 0755); err != nil {
		return Commit{}, false, fmt.Errorf("failed to create object dir: %w", err)
	}
	// Only write if not already present (content-addressable dedup)
	if _, err := os.Stat(objPath); os.IsNotExist(err) {
		if err := os.WriteFile(objPath, content, 0644); err != nil {
			return Commit{}, false, fmt.Errorf("failed to write object: %w", err)
		}
	}

	// Append to commit log
	now := time.Now().UnixMilli()
	commit := Commit{
		Hash:      contentHash,
		Parent:    parentHash,
		Timestamp: now,
		Source:    source,
	}

	logFile := filepath.Join(h.historyDir(path), "commits.jsonl")
	if err := os.MkdirAll(filepath.Dir(logFile), 0755); err != nil {
		return Commit{}, false, fmt.Errorf("failed to create log dir: %w", err)
	}

	commitJSON, err := json.Marshal(commit)
	if err != nil {
		return Commit{}, false, fmt.Errorf("failed to marshal commit: %w", err)
	}

	f, err := os.OpenFile(logFile, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0644)
	if err != nil {
		return Commit{}, false, fmt.Errorf("failed to open commit log: %w", err)
	}
	defer f.Close()

	if _, err := f.Write(append(commitJSON, '\n')); err != nil {
		return Commit{}, false, fmt.Errorf("failed to write commit: %w", err)
	}

	// Update HEAD
	if err := os.WriteFile(headFile, []byte(contentHash+"\n"), 0644); err != nil {
		return Commit{}, false, fmt.Errorf("failed to update head: %w", err)
	}

	return commit, false, nil
}

// EnsureInitialized creates the initial commit for a file if no history exists.
func (h *HistoryStore) EnsureInitialized(path string, content []byte) error {
	h.mu.Lock()
	defer h.mu.Unlock()

	headFile := filepath.Join(h.historyDir(path), "head")
	if _, err := os.Stat(headFile); err == nil {
		// Already initialized
		return nil
	}

	// Create the history directory
	dir := h.historyDir(path)
	if err := os.MkdirAll(dir, 0755); err != nil {
		return fmt.Errorf("failed to create history dir: %w", err)
	}

	contentHash := sha256Hex(content)

	// Store content object
	objPath := h.objectPath(path, contentHash)
	if err := os.MkdirAll(filepath.Dir(objPath), 0755); err != nil {
		return fmt.Errorf("failed to create object dir: %w", err)
	}
	if err := os.WriteFile(objPath, content, 0644); err != nil {
		return fmt.Errorf("failed to write object: %w", err)
	}

	// Write initial commit
	now := time.Now().UnixMilli()
	commit := Commit{
		Hash:      contentHash,
		Parent:    "",
		Timestamp: now,
		Source:    "disk",
	}

	commitJSON, err := json.Marshal(commit)
	if err != nil {
		return fmt.Errorf("failed to marshal commit: %w", err)
	}

	logFile := filepath.Join(dir, "commits.jsonl")
	if err := os.WriteFile(logFile, append(commitJSON, '\n'), 0644); err != nil {
		return fmt.Errorf("failed to write commit log: %w", err)
	}

	// Write HEAD
	if err := os.WriteFile(headFile, []byte(contentHash+"\n"), 0644); err != nil {
		return fmt.Errorf("failed to write head: %w", err)
	}

	return nil
}

// FindLCA finds the last common ancestor of two hashes by walking
// both commit chains backwards.
func (h *HistoryStore) FindLCA(path string, hash1 string, hash2 string) (string, error) {
	h.mu.RLock()
	defer h.mu.RUnlock()

	commits, err := h.getCommitsUnlocked(path)
	if err != nil {
		return "", err
	}

	// Build a map of hash -> parent for quick lookup
	parentMap := make(map[string]string)
	for _, c := range commits {
		parentMap[c.Hash] = c.Parent
	}

	// Collect all ancestors of hash1
	ancestors := make(map[string]bool)
	current := hash1
	for current != "" {
		ancestors[current] = true
		parent, ok := parentMap[current]
		if !ok {
			break
		}
		current = parent
	}

	// Walk hash2's chain until we find a common ancestor
	current = hash2
	for current != "" {
		if ancestors[current] {
			return current, nil
		}
		parent, ok := parentMap[current]
		if !ok {
			break
		}
		current = parent
	}

	return "", fmt.Errorf("no common ancestor found for %s and %s", hash1, hash2)
}

// ReconcileDiskState checks if the disk content differs from HEAD
// and auto-commits if so. Called before processing client writes
// and on file reads to capture external edits.
func (h *HistoryStore) ReconcileDiskState(path string, diskContent []byte) error {
	diskHash := sha256Hex(diskContent)

	head, err := h.GetHead(path)
	if err != nil {
		return err
	}

	if head == "" {
		// No history yet, initialize
		return h.EnsureInitialized(path, diskContent)
	}

	if diskHash != head {
		// Disk was modified externally, auto-commit
		_, _, err = h.RecordCommit(path, diskContent, head, "disk")
		return err
	}

	return nil // Already up to date
}

// DeleteHistory removes all history for a file.
func (h *HistoryStore) DeleteHistory(path string) error {
	h.mu.Lock()
	defer h.mu.Unlock()

	dir := h.historyDir(path)
	if err := os.RemoveAll(dir); err != nil && !os.IsNotExist(err) {
		return fmt.Errorf("failed to delete history: %w", err)
	}
	return nil
}
