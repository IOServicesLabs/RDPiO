package main

// Integration-style tests for the full RDPiO service: they start the real
// servers wired together by startServers (RDP proxy listener + HTTP health
// server), query the /healthz endpoint over HTTP, forward real TCP traffic
// through the proxy to an in-process echo target, and exercise the graceful
// shutdown path.

import (
	"context"
	"fmt"
	"io"
	"net"
	"net/http"
	"strconv"
	"testing"
	"time"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"

	"github.com/IOServicesLabs/RDPiO/config"
)

// startEchoTarget starts an in-process TCP echo server on 127.0.0.1:0 and
// returns its address. It stands in for a real RDP target server. The
// listener is closed when the test ends.
func startEchoTarget(t *testing.T) string {
	t.Helper()

	listener, err := net.Listen("tcp", "127.0.0.1:0")
	require.NoError(t, err)
	t.Cleanup(func() { _ = listener.Close() })

	go func() {
		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			go func(c net.Conn) {
				defer c.Close()
				_, _ = io.Copy(c, c)
			}(conn)
		}
	}()

	return listener.Addr().String()
}

// splitTargetAddr parses "host:port" into its parts.
func splitTargetAddr(t *testing.T, addr string) (string, int) {
	t.Helper()
	host, portStr, err := net.SplitHostPort(addr)
	require.NoError(t, err)
	port, err := strconv.Atoi(portStr)
	require.NoError(t, err)
	return host, port
}

// startTestService boots the full service with a free proxy port, a free
// health port, and the given target. The service is shut down when the test
// ends.
func startTestService(t *testing.T, targetHost string, targetPort int) *service {
	t.Helper()

	srv, err := startServers(config.Config{
		ProxyPort:  0, // free port
		TargetHost: targetHost,
		TargetPort: targetPort,
	}, 0 /* free health port */)
	require.NoError(t, err)

	t.Cleanup(func() {
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = srv.Shutdown(ctx)
	})
	return srv
}

// pingHealth polls the health endpoint until it answers or the deadline
// passes.
func pingHealth(t *testing.T, url string) error {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		resp, err := http.Get(url)
		if err == nil {
			resp.Body.Close()
			return nil
		}
		time.Sleep(20 * time.Millisecond)
	}
	return fmt.Errorf("health endpoint %s never came up", url)
}

// TestHealthServerServesHealthz starts the full service and queries the
// health endpoint over a real HTTP connection, mirroring
// `curl localhost:8080/healthz`.
func TestHealthServerServesHealthz(t *testing.T) {
	srv := startTestService(t, "127.0.0.1", 1)
	healthURL := "http://" + srv.httpAddr + "/healthz"

	require.NoError(t, pingHealth(t, healthURL))

	resp, err := http.Get(healthURL)
	require.NoError(t, err)
	defer resp.Body.Close()

	assert.Equal(t, http.StatusOK, resp.StatusCode)
	assert.Equal(t, "application/json", resp.Header.Get("Content-Type"))

	body, err := io.ReadAll(resp.Body)
	require.NoError(t, err)
	assert.Equal(t, `{"status":"ok"}`, string(body))
}

// TestServiceForwardsRDPTrafficEndToEnd starts a temporary echo target, the
// full proxy service in front of it, and verifies that bytes written to the
// proxy listener are forwarded to the target and echoed back to the client.
func TestServiceForwardsRDPTrafficEndToEnd(t *testing.T) {
	host, port := splitTargetAddr(t, startEchoTarget(t))
	srv := startTestService(t, host, port)

	proxyAddr := srv.proxyListener.Addr().String()

	// First connection: a round trip through the whole service.
	client, err := net.Dial("tcp", proxyAddr)
	require.NoError(t, err)
	defer client.Close()

	payload := []byte("rdp-connection-test")
	_, err = client.Write(payload)
	require.NoError(t, err)

	echoed := make([]byte, len(payload))
	_, err = io.ReadFull(client, echoed)
	require.NoError(t, err)
	assert.Equal(t, payload, echoed)

	// Second connection: the service keeps serving after the first closes.
	client2, err := net.Dial("tcp", proxyAddr)
	require.NoError(t, err)
	defer client2.Close()

	msg := "second-connection"
	_, err = client2.Write([]byte(msg))
	require.NoError(t, err)
	got := make([]byte, len(msg))
	_, err = io.ReadFull(client2, got)
	require.NoError(t, err)
	assert.Equal(t, msg, string(got))
}

// TestServiceShutdownClosesListeners verifies that Shutdown closes both the
// RDP listener and the HTTP health server so no new connections are
// accepted.
func TestServiceShutdownClosesListeners(t *testing.T) {
	host, port := splitTargetAddr(t, startEchoTarget(t))
	srv := startTestService(t, host, port)

	proxyAddr := srv.proxyListener.Addr().String()
	healthURL := "http://" + srv.httpAddr + "/healthz"

	// Sanity: both endpoints are up before shutdown.
	require.NoError(t, pingHealth(t, healthURL))

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	require.NoError(t, srv.Shutdown(ctx))

	// The proxy listener must be closed: new dials fail.
	conn, err := net.DialTimeout("tcp", proxyAddr, time.Second)
	if err == nil {
		conn.Close()
		t.Fatal("expected dial to the closed proxy listener to fail")
	}

	// The health server must be gone too.
	_, err = http.Get(healthURL)
	require.Error(t, err, "expected health request after shutdown to fail")
}

// TestServiceShutdownWaitsForInFlightConnections verifies the graceful
// shutdown contract: while a proxied connection is still open, Shutdown
// blocks; once the connection closes, Shutdown completes successfully.
func TestServiceShutdownWaitsForInFlightConnections(t *testing.T) {
	host, port := splitTargetAddr(t, startEchoTarget(t))
	srv := startTestService(t, host, port)

	// Keep a client connected through the proxy for the whole shutdown.
	client, err := net.Dial("tcp", srv.proxyListener.Addr().String())
	require.NoError(t, err)
	defer client.Close()

	hold := "hold"
	_, err = client.Write([]byte(hold))
	require.NoError(t, err)
	echoed := make([]byte, len(hold))
	_, err = io.ReadFull(client, echoed)
	require.NoError(t, err)
	assert.Equal(t, hold, string(echoed))

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	shutdownDone := make(chan error, 1)
	go func() { shutdownDone <- srv.Shutdown(ctx) }()

	// While the client is still connected, Shutdown must keep waiting.
	select {
	case err := <-shutdownDone:
		t.Fatalf("Shutdown returned while a proxy connection was in flight: %v", err)
	case <-time.After(200 * time.Millisecond):
		// Expected: shutdown is blocked on the in-flight connection.
	}

	// Closing the client lets the proxy finish and Shutdown complete.
	require.NoError(t, client.Close())

	select {
	case err := <-shutdownDone:
		require.NoError(t, err, "Shutdown should complete once the connection closes")
	case <-time.After(5 * time.Second):
		t.Fatal("Shutdown did not complete after the in-flight connection closed")
	}
}

// TestHealthPortFromEnv covers the HEALTH_PORT configuration used by the
// HTTP health server.
func TestHealthPortFromEnv(t *testing.T) {
	t.Run("defaults to 8080 when unset or empty", func(t *testing.T) {
		t.Setenv("HEALTH_PORT", "")
		assert.Equal(t, defaultHealthPort, healthPortFromEnv())
	})

	t.Run("honours a valid value", func(t *testing.T) {
		t.Setenv("HEALTH_PORT", "9090")
		assert.Equal(t, 9090, healthPortFromEnv())
	})

	t.Run("falls back on a non-numeric value", func(t *testing.T) {
		t.Setenv("HEALTH_PORT", "http")
		assert.Equal(t, defaultHealthPort, healthPortFromEnv())
	})

	t.Run("falls back on zero", func(t *testing.T) {
		t.Setenv("HEALTH_PORT", "0")
		assert.Equal(t, defaultHealthPort, healthPortFromEnv())
	})

	t.Run("falls back on an out-of-range value", func(t *testing.T) {
		t.Setenv("HEALTH_PORT", "70000")
		assert.Equal(t, defaultHealthPort, healthPortFromEnv())
	})
}

// TestStartServersFailsWhenHealthPortInUse ensures a bind conflict on the
// health port is reported as an error (and does not leak the proxy
// listener).
func TestStartServersFailsWhenHealthPortInUse(t *testing.T) {
	blocker, err := net.Listen("tcp", "127.0.0.1:0")
	require.NoError(t, err)
	defer blocker.Close()
	_, port := splitTargetAddr(t, blocker.Addr().String())

	cfg := config.Config{ProxyPort: 0, TargetHost: "127.0.0.1", TargetPort: 1}
	_, err = startServers(cfg, port)
	require.Error(t, err)
}
