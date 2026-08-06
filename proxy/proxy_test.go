package proxy

import (
	"fmt"
	"io"
	"net"
	"testing"
	"time"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

// startEchoServer starts an in-process TCP echo server on a random port
// and returns its address. The server is shut down when the test ends.
func startEchoServer(t *testing.T) string {
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

// startBannerServer starts a server that writes banner to each accepted
// connection and immediately closes it.
func startBannerServer(t *testing.T, banner string) string {
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
				_, _ = c.Write([]byte(banner))
			}(conn)
		}
	}()

	return listener.Addr().String()
}

// dialFront opens a raw TCP connection to a listener and returns it.
func dialFront(t *testing.T, addr string) net.Conn {
	t.Helper()
	conn, err := net.Dial("tcp", addr)
	require.NoError(t, err)
	return conn
}

func TestNewProxy(t *testing.T) {
	p := NewProxy("127.0.0.1:3389")
	require.NotNil(t, p)
	assert.Equal(t, "127.0.0.1:3389", p.target)
	assert.Equal(t, "127.0.0.1:3389", p.Target())
}

func TestHandleConnectionForwardsData(t *testing.T) {
	targetAddr := startEchoServer(t)
	p := NewProxy(targetAddr)

	front, err := net.Listen("tcp", "127.0.0.1:0")
	require.NoError(t, err)
	defer front.Close()

	done := make(chan struct{})
	go func() {
		defer close(done)
		conn, err := front.Accept()
		if err != nil {
			return
		}
		p.HandleConnection(conn)
	}()

	client := dialFront(t, front.Addr().String())
	defer client.Close()

	payload := []byte("hello proxy")
	_, err = client.Write(payload)
	require.NoError(t, err)

	// The echo server must bounce the exact payload back.
	echoed := make([]byte, len(payload))
	_, err = io.ReadFull(client, echoed)
	require.NoError(t, err)
	assert.Equal(t, payload, echoed)

	// Closing the client write side should let the echo server see EOF,
	// close the target connection, and let HandleConnection finish.
	require.NoError(t, client.(*net.TCPConn).CloseWrite())
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("HandleConnection did not return after client closed")
	}
}

func TestHandleConnectionReceivesServerBanner(t *testing.T) {
	targetAddr := startBannerServer(t, "RDP-SERVER-READY\n")
	p := NewProxy(targetAddr)

	front, err := net.Listen("tcp", "127.0.0.1:0")
	require.NoError(t, err)
	defer front.Close()

	done := make(chan struct{})
	go func() {
		defer close(done)
		conn, err := front.Accept()
		if err != nil {
			return
		}
		p.HandleConnection(conn)
	}()

	client := dialFront(t, front.Addr().String())
	defer client.Close()

	banner := make([]byte, len("RDP-SERVER-READY\n"))
	_, err = io.ReadFull(client, banner)
	require.NoError(t, err)
	assert.Equal(t, "RDP-SERVER-READY\n", string(banner))

	// Close the client so HandleConnection can wind down.
	require.NoError(t, client.Close())
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("HandleConnection did not return after target closed")
	}
}

func TestHandleConnectionClosesClientWhenTargetUnreachable(t *testing.T) {
	// Reserve a port and release it again so nothing is listening there.
	l, err := net.Listen("tcp", "127.0.0.1:0")
	require.NoError(t, err)
	deadAddr := l.Addr().String()
	require.NoError(t, l.Close())

	p := NewProxy(deadAddr)

	front, err := net.Listen("tcp", "127.0.0.1:0")
	require.NoError(t, err)
	defer front.Close()

	done := make(chan struct{})
	go func() {
		defer close(done)
		conn, err := front.Accept()
		if err != nil {
			return
		}
		p.HandleConnection(conn)
	}()

	client := dialFront(t, front.Addr().String())
	defer client.Close()

	// Dialing the dead target fails, so HandleConnection must close the
	// client; reading from it should then return EOF/error promptly.
	require.NoError(t, client.SetReadDeadline(time.Now().Add(5*time.Second)))
	_, err = client.Read(make([]byte, 1))
	assert.Error(t, err)

	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("HandleConnection did not return when target was unreachable")
	}
}

func TestRunForwardsDataBetweenSockets(t *testing.T) {
	targetAddr := startEchoServer(t)
	p := NewProxy(targetAddr)

	front, err := net.Listen("tcp", "127.0.0.1:0")
	require.NoError(t, err)
	defer front.Close()

	runDone := make(chan struct{})
	go func() {
		defer close(runDone)
		p.Run(front)
	}()

	client := dialFront(t, front.Addr().String())
	defer client.Close()

	// Several round trips through the proxy, including a large payload to
	// exercise buffered copies.
	for i := 0; i < 5; i++ {
		msg := fmt.Sprintf("ping-%d", i)
		_, err := client.Write([]byte(msg))
		require.NoError(t, err)
		echoed := make([]byte, len(msg))
		_, err = io.ReadFull(client, echoed)
		require.NoError(t, err)
		assert.Equal(t, msg, string(echoed))
	}

	big := make([]byte, 256*1024)
	for i := range big {
		big[i] = byte(i % 251)
	}
	_, err = client.Write(big)
	require.NoError(t, err)
	got := make([]byte, len(big))
	_, err = io.ReadFull(client, got)
	require.NoError(t, err)
	assert.Equal(t, big, got)

	// The proxy must keep serving after one connection closes.
	require.NoError(t, client.(*net.TCPConn).CloseWrite())
	second := dialFront(t, front.Addr().String())
	defer second.Close()
	_, err = second.Write([]byte("after-close"))
	require.NoError(t, err)
	echoed := make([]byte, len("after-close"))
	_, err = io.ReadFull(second, echoed)
	require.NoError(t, err)
	assert.Equal(t, "after-close", string(echoed))

	// Close the front listener; Run must return promptly.
	require.NoError(t, front.Close())
	select {
	case <-runDone:
	case <-time.After(5 * time.Second):
		t.Fatal("Run did not return after listener was closed")
	}
}

func TestRunStopsWhenListenerClosed(t *testing.T) {
	p := NewProxy("127.0.0.1:1")

	front, err := net.Listen("tcp", "127.0.0.1:0")
	require.NoError(t, err)

	done := make(chan struct{})
	go func() {
		defer close(done)
		p.Run(front)
	}()

	require.NoError(t, front.Close())
	select {
	case <-done:
	case <-time.After(5 * time.Second):
		t.Fatal("Run did not return after listener was closed")
	}
}
