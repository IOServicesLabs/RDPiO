// Package config loads the runtime configuration for the RDPiO Go proxy
// from environment variables.
package config

import (
	"os"
	"strconv"
)

// Default values used when the corresponding environment variables are
// unset or cannot be parsed.
const (
	DefaultProxyPort  = 3389
	DefaultTargetHost = "127.0.0.1"
	DefaultTargetPort = 3389
)

// Environment variable names read by Load.
const (
	EnvProxyPort  = "RDP_PROXY_PORT"
	EnvTargetHost = "RDP_TARGET_HOST"
	EnvTargetPort = "RDP_TARGET_PORT"
)

// Config holds the runtime configuration for the RDP proxy.
type Config struct {
	// ProxyPort is the port the proxy listens on for inbound RDP traffic.
	ProxyPort int
	// TargetHost is the hostname or IP address of the RDP target server.
	TargetHost string
	// TargetPort is the port of the RDP target server.
	TargetPort int
}

// Load reads configuration from the environment. It honours the variables
// RDP_PROXY_PORT, RDP_TARGET_HOST and RDP_TARGET_PORT, falling back to the
// defaults 3389, 127.0.0.1 and 3389 respectively when a variable is unset
// or empty. A port variable that is set but not a valid integer is ignored
// in favour of the default value.
func Load() Config {
	return Config{
		ProxyPort:  envInt(EnvProxyPort, DefaultProxyPort),
		TargetHost: envString(EnvTargetHost, DefaultTargetHost),
		TargetPort: envInt(EnvTargetPort, DefaultTargetPort),
	}
}

// envString returns the value of the environment variable key, or fallback
// when the variable is unset or empty.
func envString(key, fallback string) string {
	if v, ok := os.LookupEnv(key); ok && v != "" {
		return v
	}
	return fallback
}

// envInt returns the value of the environment variable key parsed as an
// integer, or fallback when the variable is unset, empty or not a valid
// integer.
func envInt(key string, fallback int) int {
	v, ok := os.LookupEnv(key)
	if !ok || v == "" {
		return fallback
	}
	n, err := strconv.Atoi(v)
	if err != nil {
		return fallback
	}
	return n
}
