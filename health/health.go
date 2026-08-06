// Package health provides the HTTP health-check endpoint for the RDPiO
// proxy service.
package health

import "net/http"

// healthzBody is the exact JSON body served by GET /healthz.
const healthzBody = `{"status":"ok"}`

// Handler returns an http.Handler exposing the service health endpoint.
// GET /healthz responds 200 with Content-Type application/json and the
// exact body {"status":"ok"}; other methods are rejected with 405 and any
// other path with 404 (both handled by http.ServeMux).
func Handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte(healthzBody))
	})
	return mux
}
