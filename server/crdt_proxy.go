package server

import (
	"fmt"
	"net/http"
	"net/http/httputil"
	"net/url"
	"strings"

	"github.com/go-chi/chi/v5"
)

func buildCrdtProxyRoutes(sidecarPort int) chi.Router {
	r := chi.NewRouter()

	target, _ := url.Parse(fmt.Sprintf("http://127.0.0.1:%d", sidecarPort))
	proxy := httputil.NewSingleHostReverseProxy(target)

	// Rewrite the request path: /.crdt/ws/foo.md -> /ws/foo.md
	originalDirector := proxy.Director
	proxy.Director = func(req *http.Request) {
		originalDirector(req)
		// The incoming path is just the wildcard part (e.g., "index.md")
		// because chi strips the mount prefix. Reconstruct as /ws/{path}
		docPath := chi.URLParam(req, "*")
		req.URL.Path = "/ws/" + docPath
		req.URL.RawPath = "/ws/" + docPath
		req.Host = target.Host
	}

	// Flush immediately for WebSocket upgrade responses
	proxy.FlushInterval = -1

	r.HandleFunc("/*", func(w http.ResponseWriter, r *http.Request) {
		docPath := chi.URLParam(r, "*")
		if docPath == "" || !strings.HasSuffix(docPath, ".md") {
			http.Error(w, "Invalid document path", http.StatusBadRequest)
			return
		}
		proxy.ServeHTTP(w, r)
	})

	return r
}
