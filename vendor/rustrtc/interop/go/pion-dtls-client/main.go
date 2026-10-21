// Minimal pion/dtls client used by the rustrtc interop test: dials the
// rustrtc DTLS server over UDP, completes the handshake, then performs one
// application-data echo round trip.
//
// Usage: pion-dtls-client <host:port>
// Prints "HANDSHAKE_OK" once connected and "ECHO_OK" after the echo round
// trip; any failure prints "FAIL: <err>" and exits non-zero.
package main

import (
	"crypto/tls"
	"fmt"
	"net"
	"os"
	"time"

	"github.com/pion/dtls/v2"
	"github.com/pion/dtls/v2/pkg/crypto/selfsign"
)

func main() {
	if len(os.Args) != 2 {
		fmt.Println("FAIL: usage: pion-dtls-client <host:port>")
		os.Exit(2)
	}
	addr, err := net.ResolveUDPAddr("udp", os.Args[1])
	if err != nil {
		fmt.Printf("FAIL: resolve: %v\n", err)
		os.Exit(1)
	}

	cert, err := selfsign.GenerateSelfSigned()
	if err != nil {
		fmt.Printf("FAIL: selfsign: %v\n", err)
		os.Exit(1)
	}

	// CipherSuites left nil: pion's default list (mirrors pion/webrtc),
	// whose first entry is TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256.
	config := &dtls.Config{
		Certificates:       []tls.Certificate{cert},
		InsecureSkipVerify: true,
	}

	conn, err := dtls.Dial("udp", addr, config)
	if err != nil {
		fmt.Printf("FAIL: handshake: %v\n", err)
		os.Exit(1)
	}
	defer conn.Close()
	fmt.Println("HANDSHAKE_OK")

	_ = conn.SetDeadline(time.Now().Add(10 * time.Second))
	if _, err := conn.Write([]byte("ping")); err != nil {
		fmt.Printf("FAIL: write: %v\n", err)
		os.Exit(1)
	}
	buf := make([]byte, 32)
	n, err := conn.Read(buf)
	if err != nil {
		fmt.Printf("FAIL: read: %v\n", err)
		os.Exit(1)
	}
	if string(buf[:n]) != "pong" {
		fmt.Printf("FAIL: unexpected echo %q\n", buf[:n])
		os.Exit(1)
	}
	fmt.Println("ECHO_OK")
}
