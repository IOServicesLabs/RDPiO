// Command rdpiogo is the RDPiO TCP proxy service. It listens for inbound
// RDP connections on the port given by RDP_PROXY_PORT (default 3389) and
// forwards them to the RDP target configured via RDP_TARGET_HOST /
// RDP_TARGET_PORT (defaults 127.0.0.1:3389). A small HTTP server on
// HEALTH_PORT (default 8080) exposes a /healthz endpoint returning
// {"status":"ok"} for health checks.
//
// The process shuts down gracefully on SIGINT/SIGTERM: it stops accepting
// new RDP connections, aborts the HTTP server, and waits for all in-flight
// proxy connections to finish.
package main

import (
	"context"
	"errors"
	"fmt"
	"net"
	"net/http"
	"os"
	"os/signal"
	"strconv"
	"syscall"
	"time"

	"github.com/sirupsen/logrus"

	"github.com/IOServicesLabs/RDPiO/config"
	"github.com/IOServicesLabs/RDPiO/health"
	"github.com/IOServicesLabs/RDPiO/proxy"
)

// defaultHealthPort is the port the HTTP health server listens on unless
// HEALTH_PORT is set.
const defaultHealthPort = 8080

// shutdownTimeout bounds how long graceful shutdown may take (waiting for
// the HTTP server and in-flight proxy connections) before giving up.
const shutdownTimeout = 10 * time.Second

func main() {
	logrus.SetFormatter(&logrus.TextFormatter{FullTimestamp: true})

	// NotifyContext cancels ctx on the first SIGINT or SIGTERM, which is
	// the trigger for graceful shutdown.
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	cfg := config.Load()
	logrus.WithFields(logrus.Fields{
		"proxy_port":  cfg.ProxyPort,
		"target_host": cfg.TargetHost,
		"target_port": cfg.TargetPort,
	}).Info("loaded configuration")

	srv, err := startServers(cfg, healthPortFromEnv())
	if err != nil {
		logrus.WithError(err).Fatal("failed to start servers")
	}

	logrus.Info("RDPiO proxy service started; waiting for SIGINT/SIGTERM")

	select {
	case <-ctx.Done():
		logrus.Info("shutdown signal received, starting graceful shutdown")
	case err := <-srv.httpErr:
		if err != nil {
			logrus.WithError(err).Fatal("health server failed")
		}
		return
	}

	shutdownCtx, cancel := context.WithTimeout(context.Background(), shutdownTimeout)
	defer cancel()
	if err := srv.Shutdown(shutdownCtx); err != nil {
		logrus.WithError(err).Error("graceful shutdown did not complete cleanly")
		os.Exit(1)
	}
	logrus.Info("shutdown complete")
}

// healthPortFromEnv returns the port for the HTTP health server, honouring
// the HEALTH_PORT environment variable and falling back to 8080 when it is
// unset or invalid.
func healthPortFromEnv() int {
	raw := os.Getenv("HEALTH_PORT")
	if raw == "" {
		return defaultHealthPort
	}
	port, err := strconv.Atoi(raw)
	if err != nil || port < 1 || port > 65535 {
		logrus.WithField("HEALTH_PORT", raw).
			Warnf("invalid HEALTH_PORT %q, using default %d", raw, defaultHealthPort)
		return defaultHealthPort
	}
	return port
}

// service bundles the running servers of the RDPiO proxy so they can be
// shut down together and inspected by tests.
type service struct {
	// proxyListener accepts inbound RDP connections.
	proxyListener net.Listener
	// proxy forwards accepted connections to the RDP target.
	proxy *proxy.Proxy
	// proxyDone is closed once the proxy accept loop has returned.
	proxyDone chan struct{}
	// http is the health-check HTTP server.
	http *http.Server
	// httpAddr is the address the HTTP server is bound to.
	httpAddr string
	// httpErr receives the terminal result of the HTTP server.
	httpErr chan error
}

// startServers starts the RDP proxy listener (on cfg.ProxyPort) and the
// HTTP health server (on healthPort), then returns a *service that can be
// used to shut both down. The proxy forwards every accepted connection to
// cfg.TargetHost:cfg.TargetPort.
func startServers(cfg config.Config, healthPort int) (*service, error) {
	proxyAddr := net.JoinHostPort("", strconv.Itoa(cfg.ProxyPort))
	proxyListener, err := net.Listen("tcp", proxyAddr)
	if err != nil {
		return nil, fmt.Errorf("listen for RDP traffic on %s: %w", proxyAddr, err)
	}

	httpListener, err := net.Listen("tcp", net.JoinHostPort("", strconv.Itoa(healthPort)))
	if err != nil {
		_ = proxyListener.Close()
		return nil, fmt.Errorf("listen for health checks on port %d: %w", healthPort, err)
	}

	target := net.JoinHostPort(cfg.TargetHost, strconv.Itoa(cfg.TargetPort))
	p := proxy.NewProxy(target)

	srv := &service{
		proxyListener: proxyListener,
		proxy:         p,
		proxyDone:     make(chan struct{}),
		http:          &http.Server{Handler: health.Handler()},
		httpAddr:      httpListener.Addr().String(),
		httpErr:       make(chan error, 1),
	}

	// Accept loop: runs until the listener is closed.
	go func() {
		p.Run(proxyListener)
		close(srv.proxyDone)
	}()

	// Health server: reports its terminal result (nil when shut down
	// cleanly, the error otherwise) via httpErr.
	go func() {
		err := srv.http.Serve(httpListener)
		if err != nil && !errors.Is(err, http.ErrServerClosed) {
			srv.httpErr <- err
			return
		}
		srv.httpErr <- nil
	}()

	logrus.WithFields(logrus.Fields{
		"addr":   proxyListener.Addr().String(),
		"target": target,
	}).Info("RDP proxy listening")

	logrus.WithField("addr", srv.httpAddr).Info("health server listening")

	return srv, nil
}

// Shutdown stops the service gracefully: it closes the RDP listener (which
// makes the accept loop return), shuts down the HTTP server, and waits for
// all in-flight proxy connections to finish. It respects the context
// deadline and reports an error if shutdown cannot complete in time.
func (s *service) Shutdown(ctx context.Context) error {
	// Stop accepting new RDP connections; Run returns promptly and
	// proxyDone is closed.
	if err := s.proxyListener.Close(); err != nil && !errors.Is(err, net.ErrClosed) {
		return fmt.Errorf("close RDP listener: %w", err)
	}
	<-s.proxyDone

	// Abort the HTTP health server.
	if err := s.http.Shutdown(ctx); err != nil {
		return fmt.Errorf("shut down health server: %w", err)
	}

	// Wait for all in-flight proxy connections to finish, bounded by the
	// context deadline so an idle connection cannot block shutdown forever.
	done := make(chan struct{})
	go func() {
		s.proxy.Wait()
		close(done)
	}()
	select {
	case <-done:
		return nil
	case <-ctx.Done():
		return fmt.Errorf("timed out waiting for proxy connections: %w", ctx.Err())
	}
}
