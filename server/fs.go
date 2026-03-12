package server

import (
	"fmt"
	"io"
	"log"
	"net/http"
	"net/url"
	"strconv"
	"strings"

	"github.com/go-chi/chi/v5"
	"github.com/go-chi/render"
)

// buildFsRoutes creates the filesystem API routes
func buildFsRoutes() http.Handler {
	fsRouter := chi.NewRouter()

	// File list endpoint
	fsRouter.Get("/", handleFsList)

	// File operations
	fsRouter.Get("/*", handleFsGet)
	fsRouter.Put("/*", handleFsPut)
	fsRouter.Delete("/*", handleFsDelete)
	fsRouter.Options("/*", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Allow", "GET, PUT, DELETE, OPTIONS")
		w.WriteHeader(http.StatusOK)
	})

	return fsRouter
}

func handleFsList(w http.ResponseWriter, r *http.Request) {
	spaceConfig := spaceConfigFromContext(r.Context())
	if r.Header.Get("X-Sync-Mode") != "" {
		// Handle direct requests for JSON representation of file list
		files, err := spaceConfig.SpacePrimitives.FetchFileList()
		if err != nil {
			http.Error(w, err.Error(), http.StatusInternalServerError)
			return
		}
		w.Header().Set("X-Space-Path", spaceConfig.SpaceFolderPath)
		w.Header().Set("Cache-Control", "no-cache")
		render.JSON(w, r, files)
	} else {
		// Otherwise, redirect to the UI
		http.Redirect(w, r, "/", http.StatusTemporaryRedirect)
	}
}

// handleFsGet handles GET requests for individual files
func handleFsGet(w http.ResponseWriter, r *http.Request) {
	path := DecodeURLParam(r, "*")
	spaceConfig := spaceConfigFromContext(r.Context())

	// log.Printf("Got this path: %s", path)

	if r.Header.Get("X-Get-Meta") != "" {
		// Getting meta via GET request
		meta, err := spaceConfig.SpacePrimitives.GetFileMeta(path)
		if err != nil {
			if err == ErrNotFound {
				http.NotFound(w, r)
			} else {
				http.Error(w, err.Error(), http.StatusInternalServerError)
			}
			return
		}

		// For .md meta requests, reconcile disk state and include content hash
		if strings.HasSuffix(path, ".md") && spaceConfig.HistoryStore != nil {
			// Read file content to reconcile disk state (ensures history is initialized)
			if data, _, readErr := spaceConfig.SpacePrimitives.ReadFile(path); readErr == nil {
				if err := spaceConfig.HistoryStore.ReconcileDiskState(path, data); err != nil {
					log.Printf("Failed to reconcile disk state for %s: %v", path, err)
				}
				hash := sha256Hex(data)
				w.Header().Set("X-Content-Hash", hash)
				log.Printf("[history] GET meta %s: X-Content-Hash=%s", path, hash[:min(len(hash), 12)])
			} else {
				log.Printf("[history] GET meta %s: ReadFile failed: %v", path, readErr)
			}
		}

		setFileMetaHeaders(w, meta)
		w.WriteHeader(http.StatusOK)
		return
	}

	// Read file content
	data, meta, err := spaceConfig.SpacePrimitives.ReadFile(path)
	if err != nil {
		if err == ErrNotFound {
			http.NotFound(w, r)
		} else {
			http.Error(w, err.Error(), http.StatusInternalServerError)
		}
		return
	}

	// For .md files, reconcile disk state and add content hash
	if strings.HasSuffix(path, ".md") && spaceConfig.HistoryStore != nil {
		if err := spaceConfig.HistoryStore.ReconcileDiskState(path, data); err != nil {
			log.Printf("Failed to reconcile disk state for %s: %v", path, err)
		}
		w.Header().Set("X-Content-Hash", sha256Hex(data))
	}

	setFileMetaHeaders(w, meta)
	w.WriteHeader(http.StatusOK)
	w.Write(data)
}

// handleFsPut handles PUT requests for writing files
func handleFsPut(w http.ResponseWriter, r *http.Request) {
	path := DecodeURLParam(r, "*")
	spaceConfig := spaceConfigFromContext(r.Context())

	// Read request body
	body, err := io.ReadAll(r.Body)
	if err != nil {
		http.Error(w, "Failed to read request body", http.StatusBadRequest)
		return
	}

	// For .md files with history enabled, use versioned write
	if strings.HasSuffix(path, ".md") && spaceConfig.HistoryStore != nil {
		parentHash := r.Header.Get("X-Parent-Hash")
		if parentHash == "" {
			log.Printf("[history] PUT %s: no X-Parent-Hash (legacy/first write)", path)
		} else {
			log.Printf("[history] PUT %s: X-Parent-Hash=%s", path, parentHash[:min(len(parentHash), 12)])
		}
		result, err := handleVersionedWrite(spaceConfig, path, body, parentHash)
		if err != nil {
			log.Printf("Versioned write failed for %s: %v\n", path, err)
			http.Error(w, "Write failed", http.StatusInternalServerError)
			return
		}

		if result.HasConflict {
			setFileMetaHeaders(w, result.Meta)
			w.Header().Set("X-Content-Hash", result.Hash)
			w.WriteHeader(http.StatusConflict)
			w.Write(result.MergedContent)
			return
		}

		setFileMetaHeaders(w, result.Meta)
		w.Header().Set("X-Content-Hash", result.Hash)
		w.WriteHeader(http.StatusOK)
		w.Write([]byte("OK"))
		return
	}

	// Non-.md files or no history: existing behavior
	meta, err := spaceConfig.SpacePrimitives.WriteFile(path, body, getFileMetaFromHeaders(r.Header, path))
	if err != nil {
		log.Printf("Write failed: %v\n", err)
		http.Error(w, "Write failed", http.StatusInternalServerError)
		return
	}

	setFileMetaHeaders(w, meta)
	w.WriteHeader(http.StatusOK)
	w.Write([]byte("OK"))
}

// handleVersionedWrite processes a write with version history and merge logic.
func handleVersionedWrite(spaceConfig *SpaceConfig, path string, content []byte, parentHash string) (*VersionedWriteResult, error) {
	h := spaceConfig.HistoryStore

	// First, reconcile any external disk changes
	currentData, _, readErr := spaceConfig.SpacePrimitives.ReadFile(path)
	if readErr == nil {
		if err := h.ReconcileDiskState(path, currentData); err != nil {
			log.Printf("Failed to reconcile disk state for %s: %v", path, err)
		}
	} else if readErr == ErrNotFound {
		// New file — will be initialized below
	}

	// Get current HEAD
	currentHead, err := h.GetHead(path)
	if err != nil {
		return nil, err
	}

	// No history yet — initialize and fast-forward
	if currentHead == "" {
		if readErr == nil {
			// Existing file without history
			if err := h.EnsureInitialized(path, currentData); err != nil {
				return nil, err
			}
			currentHead, _ = h.GetHead(path)
		} else {
			// Brand new file
			meta, err := spaceConfig.SpacePrimitives.WriteFile(path, content, nil)
			if err != nil {
				return nil, err
			}
			if err := h.EnsureInitialized(path, content); err != nil {
				return nil, err
			}
			head, _ := h.GetHead(path)
			return &VersionedWriteResult{Hash: head, Meta: meta}, nil
		}
	}

	// No parent hash (legacy client) — treat as fast-forward
	if parentHash == "" {
		meta, err := spaceConfig.SpacePrimitives.WriteFile(path, content, nil)
		if err != nil {
			return nil, err
		}
		commit, _, err := h.RecordCommit(path, content, currentHead, "client")
		if err != nil {
			return nil, err
		}
		return &VersionedWriteResult{Hash: commit.Hash, Meta: meta}, nil
	}

	// Fast-forward: parent matches HEAD
	if parentHash == currentHead {
		contentHash := sha256Hex(content)
		if contentHash == currentHead {
			// Content unchanged — no-op
			meta, _ := spaceConfig.SpacePrimitives.GetFileMeta(path)
			return &VersionedWriteResult{Hash: currentHead, Meta: meta}, nil
		}

		// Write to disk
		meta, err := spaceConfig.SpacePrimitives.WriteFile(path, content, nil)
		if err != nil {
			return nil, err
		}

		commit, _, err := h.RecordCommit(path, content, currentHead, "client")
		if err != nil {
			return nil, err
		}
		return &VersionedWriteResult{Hash: commit.Hash, Meta: meta}, nil
	}

	// Diverged: need to merge
	lca, err := h.FindLCA(path, parentHash, currentHead)
	if err != nil {
		// Can't find LCA — fall back to using parent as base
		lca = parentHash
	}

	// Get content for base, ours (server HEAD), and theirs (client)
	baseContent, err := h.GetContent(path, lca)
	if err != nil {
		return nil, fmt.Errorf("failed to get LCA content: %w", err)
	}
	headContent, err := h.GetContent(path, currentHead)
	if err != nil {
		return nil, fmt.Errorf("failed to get HEAD content: %w", err)
	}

	merged, hasConflict, err := ThreeWayMerge(baseContent, headContent, content)
	if err != nil {
		return nil, fmt.Errorf("merge failed: %w", err)
	}

	if hasConflict {
		// Return conflict — don't write to disk or record commit
		meta, _ := spaceConfig.SpacePrimitives.GetFileMeta(path)
		return &VersionedWriteResult{
			Hash:          currentHead,
			Meta:          meta,
			HasConflict:   true,
			MergedContent: merged,
		}, nil
	}

	// Clean merge — write result to disk and record commit
	meta, err := spaceConfig.SpacePrimitives.WriteFile(path, merged, nil)
	if err != nil {
		return nil, err
	}

	commit, _, err := h.RecordCommit(path, merged, currentHead, "client")
	if err != nil {
		return nil, err
	}

	return &VersionedWriteResult{Hash: commit.Hash, Meta: meta}, nil
}

// handleFsDelete handles DELETE requests for removing files
func handleFsDelete(w http.ResponseWriter, r *http.Request) {
	path := DecodeURLParam(r, "*")
	spaceConfig := spaceConfigFromContext(r.Context())

	if err := spaceConfig.SpacePrimitives.DeleteFile(path); err != nil {
		if err == ErrNotFound {
			http.NotFound(w, r)
		} else {
			log.Printf("Error deleting file: %v\n", err)
			http.Error(w, err.Error(), http.StatusInternalServerError)
		}
		return
	}

	// Clean up version history for deleted .md files
	if strings.HasSuffix(path, ".md") && spaceConfig.HistoryStore != nil {
		if err := spaceConfig.HistoryStore.DeleteHistory(path); err != nil {
			log.Printf("Failed to delete history for %s: %v", path, err)
		}
	}

	w.WriteHeader(http.StatusOK)
	w.Write([]byte("OK"))
}

// setFileMetaHeaders sets HTTP headers based on FileMeta
func setFileMetaHeaders(w http.ResponseWriter, meta FileMeta) {
	w.Header().Set("Content-Type", meta.ContentType)
	w.Header().Set("X-Created", strconv.FormatInt(meta.Created, 10))
	w.Header().Set("X-Last-Modified", strconv.FormatInt(meta.LastModified, 10))
	w.Header().Set("X-Content-Length", strconv.FormatInt(meta.Size, 10))
	w.Header().Set("X-Permission", meta.Perm)
	w.Header().Set("Cache-Control", "no-cache")
}

// Build FileMeta from HTTP headers (reverse of setFileMetaHeaders)
func getFileMetaFromHeaders(h http.Header, path string) *FileMeta {
	var err error

	fm := &FileMeta{
		Name:        path,
		ContentType: h.Get("Content-Type"),
		Perm:        h.Get("X-Permission"),
	}
	if fm.Perm == "" {
		fm.Perm = "ro"
	}
	if h.Get("X-Content-Length") != "" {
		fm.Size, err = strconv.ParseInt(h.Get("X-Content-Length"), 10, 64)
		if err != nil {
			log.Printf("Could not parse content length: %v", err)
		}
	} else if h.Get("Content-Length") != "" {
		fm.Size, err = strconv.ParseInt(h.Get("Content-Length"), 10, 64)
		if err != nil {
			log.Printf("Could not parse content length: %v", err)
		}
	}
	if h.Get("X-Created") != "" {
		fm.Created, err = strconv.ParseInt(h.Get("X-Created"), 10, 64)
		if err != nil {
			log.Printf("Could not parse created time: %v", err)
		}
	}
	if h.Get("X-Last-Modified") != "" {
		fm.LastModified, err = strconv.ParseInt(h.Get("X-Last-Modified"), 10, 64)
		if err != nil {
			log.Printf("Could not parse modified time: %v", err)
		}
	}

	return fm
}

func DecodeURLParam(r *http.Request, name string) string {
	// Source: https://github.com/go-chi/chi/issues/642
	value := chi.URLParam(r, name)
	if r.URL.RawPath != "" {
		value, _ = url.PathUnescape(value)
	}
	return value
}
