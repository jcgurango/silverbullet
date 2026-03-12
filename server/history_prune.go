package server

import (
	"encoding/json"
	"log"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// StartPruner starts a background goroutine that periodically prunes old history.
func (h *HistoryStore) StartPruner(maxAge time.Duration, interval time.Duration) {
	go func() {
		// Initial delay to avoid startup overhead
		time.Sleep(30 * time.Second)
		for {
			h.PruneAll(maxAge)
			time.Sleep(interval)
		}
	}()
}

// PruneAll walks the .history directory and prunes old commits from all files.
func (h *HistoryStore) PruneAll(maxAge time.Duration) {
	historyRoot := filepath.Join(h.rootPath, ".history")
	if _, err := os.Stat(historyRoot); os.IsNotExist(err) {
		return
	}

	err := filepath.Walk(historyRoot, func(path string, info os.FileInfo, err error) error {
		if err != nil {
			return nil // skip errors
		}

		// Look for commits.jsonl files
		if info.Name() == "commits.jsonl" {
			dir := filepath.Dir(path)
			// Extract the file path from the history dir
			relPath, err := filepath.Rel(historyRoot, dir)
			if err != nil {
				return nil
			}
			// Remove .versions suffix to get the original file path
			filePath := strings.TrimSuffix(relPath, ".versions")
			if filePath != relPath {
				if err := h.Prune(filePath, maxAge); err != nil {
					log.Printf("Failed to prune history for %s: %v", filePath, err)
				}
			}
		}
		return nil
	})

	if err != nil {
		log.Printf("Error walking history directory: %v", err)
	}
}

// Prune removes commits older than maxAge for a specific file.
// Keeps at least one commit (the current HEAD) regardless of age.
func (h *HistoryStore) Prune(path string, maxAge time.Duration) error {
	h.mu.Lock()
	defer h.mu.Unlock()

	commits, err := h.getCommitsUnlocked(path)
	if err != nil || len(commits) <= 1 {
		return err
	}

	cutoff := time.Now().Add(-maxAge).UnixMilli()

	// Find which commits to keep
	var keptCommits []Commit
	referencedHashes := make(map[string]bool)

	// Always keep the last commit (HEAD)
	headCommit := commits[len(commits)-1]
	referencedHashes[headCommit.Hash] = true

	for _, c := range commits {
		if c.Timestamp >= cutoff || c.Hash == headCommit.Hash {
			keptCommits = append(keptCommits, c)
			referencedHashes[c.Hash] = true
			// Keep parent hash object too if it's referenced by a kept commit
			if c.Parent != "" {
				referencedHashes[c.Parent] = true
			}
		}
	}

	if len(keptCommits) == len(commits) {
		return nil // nothing to prune
	}

	// Fix up the parent of the oldest kept commit to "" since we're pruning its ancestors
	if len(keptCommits) > 0 {
		// Find the oldest kept commit that references a pruned parent
		for i := range keptCommits {
			parentKept := false
			for _, k := range keptCommits {
				if k.Hash == keptCommits[i].Parent {
					parentKept = true
					break
				}
			}
			if !parentKept {
				keptCommits[i].Parent = ""
			}
		}
	}

	// Rewrite commits.jsonl
	logFile := filepath.Join(h.historyDir(path), "commits.jsonl")
	var buf []byte
	for _, c := range keptCommits {
		line, err := json.Marshal(c)
		if err != nil {
			return err
		}
		buf = append(buf, line...)
		buf = append(buf, '\n')
	}
	if err := os.WriteFile(logFile, buf, 0644); err != nil {
		return err
	}

	// Clean up unreferenced object files
	objectsDir := filepath.Join(h.historyDir(path), "objects")
	if _, err := os.Stat(objectsDir); os.IsNotExist(err) {
		return nil
	}

	filepath.Walk(objectsDir, func(objPath string, info os.FileInfo, err error) error {
		if err != nil || info.IsDir() {
			return nil
		}
		hash := info.Name()
		if !referencedHashes[hash] {
			os.Remove(objPath)
		}
		return nil
	})

	// Clean up empty fan-out directories
	filepath.Walk(objectsDir, func(dirPath string, info os.FileInfo, err error) error {
		if err != nil || !info.IsDir() || dirPath == objectsDir {
			return nil
		}
		entries, err := os.ReadDir(dirPath)
		if err == nil && len(entries) == 0 {
			os.Remove(dirPath)
		}
		return nil
	})

	log.Printf("Pruned history for %s: %d -> %d commits", path, len(commits), len(keptCommits))
	return nil
}
