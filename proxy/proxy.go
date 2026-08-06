// Package proxy implements a lightweight TCP forwarder used to relay
// RDP traffic from a local listener to a remote target host.
package proxy

import (
	"errors"
	"io"
	"net"
	"sync"

	"github.com/sirupsen/logrus"
)

// Proxy forwards inbound TCP connections to a fixed target address
// given as "host:port".
type Proxy struct {
	target string

	// wg tracks every connection handled through Run so callers can wait
	// for in-flight connections to finish during graceful shutdown.
	wg sync.WaitGroup
}

// NewProxy returns a Proxy that forwards client connections to target
// (formatted as "host:port").
func NewProxy(target string) *Proxy {
	return &Proxy{target: target}
}

// Target returns the target address this proxy forwards to.
func (p *Proxy) Target() string {
	return p.target
}

// HandleConnection establishes a connection to the target and pipes data
// bidirectionally between client and target until either side closes.
// Both connections are closed before HandleConnection returns.
func (p *Proxy) HandleConnection(client net.Conn) {
	defer client.Close()

	target, err := net.Dial("tcp", p.target)
	if err != nil {
		logrus.WithError(err).
			WithField("target", p.target).
			WithField("client", client.RemoteAddr().String()).
			Error("proxy: failed to dial target")
		return
	}
	defer target.Close()

	logrus.WithFields(logrus.Fields{
		"client": client.RemoteAddr().String(),
		"target": p.target,
	}).Debug("proxy: connection established")

	// Copy in both directions concurrently. When one direction finishes
	// (EOF or error), half-close the corresponding write side so the
	// other end observes the shutdown and the second copy can complete.
	done := make(chan struct{}, 2)

	go func() {
		defer func() { done <- struct{}{} }()
		if _, err := io.Copy(target, client); err != nil {
			logrus.WithError(err).
				WithField("client", client.RemoteAddr().String()).
				Debug("proxy: client->target copy ended with error")
		}
		if tcp, ok := target.(*net.TCPConn); ok {
			_ = tcp.CloseWrite()
		}
	}()

	go func() {
		defer func() { done <- struct{}{} }()
		if _, err := io.Copy(client, target); err != nil {
			logrus.WithError(err).
				WithField("client", client.RemoteAddr().String()).
				Debug("proxy: target->client copy ended with error")
		}
		if tcp, ok := client.(*net.TCPConn); ok {
			_ = tcp.CloseWrite()
		}
	}()

	<-done
	<-done

	logrus.WithFields(logrus.Fields{
		"client": client.RemoteAddr().String(),
		"target": p.target,
	}).Debug("proxy: connection closed")
}

// Wait blocks until every connection accepted by Run has been fully
// handled and closed. It is intended to be called after the listener has
// been closed (which makes Run return), so the caller can shut down the
// process without cutting in-flight connections short.
func (p *Proxy) Wait() {
	p.wg.Wait()
}

// Run accepts connections on listener and handles each one in its own
// goroutine. It blocks until the listener is closed (or fails), at which
// point it returns so the caller can shut down.
func (p *Proxy) Run(listener net.Listener) {
	for {
		conn, err := listener.Accept()
		if err != nil {
			if errors.Is(err, net.ErrClosed) {
				logrus.Info("proxy: listener closed, stopping accept loop")
				return
			}
			var ne net.Error
			if errors.As(err, &ne) && ne.Timeout() {
				// Transient timeout (e.g. a listener with deadlines):
				// keep accepting.
				continue
			}
			logrus.WithError(err).Error("proxy: accept failed, stopping accept loop")
			return
		}
		// Register the connection with the WaitGroup before spawning the
		// goroutine so that Wait() can never miss a connection that was
		// already accepted.
		p.wg.Add(1)
		go func() {
			defer p.wg.Done()
			p.HandleConnection(conn)
		}()
	}
}
