package health

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

// TestHealthzReturnsStatusOK exercises the endpoint over a real HTTP
// server, mirroring what `curl localhost:8080/healthz` does.
func TestHealthzReturnsStatusOK(t *testing.T) {
	srv := httptest.NewServer(Handler())
	defer srv.Close()

	resp, err := http.Get(srv.URL + "/healthz")
	require.NoError(t, err)
	defer resp.Body.Close()

	assert.Equal(t, http.StatusOK, resp.StatusCode)
	assert.Equal(t, "application/json", resp.Header.Get("Content-Type"))

	var payload map[string]string
	require.NoError(t, json.NewDecoder(resp.Body).Decode(&payload))
	assert.Equal(t, map[string]string{"status": "ok"}, payload)
}

// TestHealthzExactBody verifies the exact body bytes served by the
// handler, so curl output matches {"status":"ok"} precisely.
func TestHealthzExactBody(t *testing.T) {
	req := httptest.NewRequest(http.MethodGet, "/healthz", nil)
	rec := httptest.NewRecorder()

	Handler().ServeHTTP(rec, req)

	assert.Equal(t, http.StatusOK, rec.Code)
	assert.Equal(t, "application/json", rec.Header().Get("Content-Type"))
	assert.JSONEq(t, `{"status":"ok"}`, rec.Body.String())
}

// TestHealthzRejectsNonGET ensures the endpoint only serves GET requests.
func TestHealthzRejectsNonGET(t *testing.T) {
	for _, method := range []string{http.MethodPost, http.MethodPut, http.MethodDelete} {
		req := httptest.NewRequest(method, "/healthz", nil)
		rec := httptest.NewRecorder()

		Handler().ServeHTTP(rec, req)

		assert.Equal(t, http.StatusMethodNotAllowed, rec.Code, "method %s", method)
		assert.Contains(t, rec.Header().Get("Allow"), http.MethodGet, "method %s", method)
	}
}

// TestUnknownPathReturnsNotFound ensures the handler does not answer on
// unrelated paths.
func TestUnknownPathReturnsNotFound(t *testing.T) {
	req := httptest.NewRequest(http.MethodGet, "/", nil)
	rec := httptest.NewRecorder()

	Handler().ServeHTTP(rec, req)

	assert.Equal(t, http.StatusNotFound, rec.Code)
}
