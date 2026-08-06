package config

import (
	"os"
	"testing"

	"github.com/stretchr/testify/assert"
	"github.com/stretchr/testify/require"
)

// unsetEnv removes the given environment variables for the duration of the
// test, restoring their previous values afterwards.
func unsetEnv(t *testing.T, keys ...string) {
	t.Helper()
	for _, key := range keys {
		old, had := os.LookupEnv(key)
		require.NoError(t, os.Unsetenv(key), "failed to unset %s", key)
		t.Cleanup(func(k string, prev string, existed bool) func() {
			return func() {
				if existed {
					_ = os.Setenv(k, prev)
				} else {
					_ = os.Unsetenv(k)
				}
			}
		}(key, old, had))
	}
}

func TestLoadDefaults(t *testing.T) {
	unsetEnv(t, EnvProxyPort, EnvTargetHost, EnvTargetPort)

	cfg := Load()

	assert.Equal(t, 3389, cfg.ProxyPort, "ProxyPort should default to 3389")
	assert.Equal(t, "127.0.0.1", cfg.TargetHost, "TargetHost should default to 127.0.0.1")
	assert.Equal(t, 3389, cfg.TargetPort, "TargetPort should default to 3389")
}

func TestLoadEnvOverrides(t *testing.T) {
	t.Setenv(EnvProxyPort, "13389")
	t.Setenv(EnvTargetHost, "192.168.1.50")
	t.Setenv(EnvTargetPort, "13390")

	cfg := Load()

	assert.Equal(t, 13389, cfg.ProxyPort, "ProxyPort should be read from RDP_PROXY_PORT")
	assert.Equal(t, "192.168.1.50", cfg.TargetHost, "TargetHost should be read from RDP_TARGET_HOST")
	assert.Equal(t, 13390, cfg.TargetPort, "TargetPort should be read from RDP_TARGET_PORT")
}

func TestLoadPartialOverrides(t *testing.T) {
	unsetEnv(t, EnvProxyPort, EnvTargetPort)
	t.Setenv(EnvTargetHost, "rdp.example.com")

	cfg := Load()

	assert.Equal(t, 3389, cfg.ProxyPort, "unset RDP_PROXY_PORT should fall back to default")
	assert.Equal(t, "rdp.example.com", cfg.TargetHost, "TargetHost should honour the override")
	assert.Equal(t, 3389, cfg.TargetPort, "unset RDP_TARGET_PORT should fall back to default")
}

func TestLoadInvalidPortFallsBackToDefault(t *testing.T) {
	t.Setenv(EnvProxyPort, "not-a-port")
	t.Setenv(EnvTargetPort, "")

	cfg := Load()

	assert.Equal(t, 3389, cfg.ProxyPort, "invalid RDP_PROXY_PORT should fall back to default")
	assert.Equal(t, 3389, cfg.TargetPort, "empty RDP_TARGET_PORT should fall back to default")
}

func TestLoadEmptyTargetHostFallsBackToDefault(t *testing.T) {
	t.Setenv(EnvTargetHost, "")

	cfg := Load()

	assert.Equal(t, "127.0.0.1", cfg.TargetHost, "empty RDP_TARGET_HOST should fall back to default")
}

func TestLoadZeroPortOverride(t *testing.T) {
	t.Setenv(EnvProxyPort, "0")
	t.Setenv(EnvTargetPort, "0")

	cfg := Load()

	assert.Equal(t, 0, cfg.ProxyPort, "a valid numeric 0 must be honoured, not treated as unset")
	assert.Equal(t, 0, cfg.TargetPort, "a valid numeric 0 must be honoured, not treated as unset")
}
