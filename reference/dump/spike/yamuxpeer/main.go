// Command yamuxpeer is the hashicorp/yamux side of the Phase 0 interop spike.
//
//	yamuxpeer server <unix-socket>   accept one session; echo every stream the peer opens
//	yamuxpeer client <unix-socket>   open N streams concurrently, each sending SIZE bytes,
//	                                 and verify the echo; then idle IDLE seconds and repeat once
//
// Either way the Rust side plays the other role with identical parameters.
package main

import (
	"bytes"
	"crypto/sha256"
	"fmt"
	"io"
	"net"
	"os"
	"strconv"
	"sync"
	"time"

	"github.com/hashicorp/yamux"
)

func env(name string, def int) int {
	if v, err := strconv.Atoi(os.Getenv(name)); err == nil {
		return v
	}
	return def
}

func pattern(stream, size int) []byte {
	b := make([]byte, size)
	for i := range b {
		b[i] = byte((i*31 + stream*7) % 251)
	}
	return b
}

func echo(s net.Conn) {
	defer s.Close()
	_, _ = io.Copy(s, s)
}

func runServer(path string) error {
	_ = os.Remove(path)
	l, err := net.Listen("unix", path)
	if err != nil {
		return err
	}
	conn, err := l.Accept()
	if err != nil {
		return err
	}
	sess, err := yamux.Server(conn, nil)
	if err != nil {
		return err
	}
	for {
		s, err := sess.Accept()
		if err != nil {
			fmt.Println("server: session ended:", err)
			return nil
		}
		go echo(s)
	}
}

func round(sess *yamux.Session, streams, size int) error {
	var wg sync.WaitGroup
	errs := make(chan error, streams)
	for i := range streams {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			s, err := sess.Open()
			if err != nil {
				errs <- fmt.Errorf("open %d: %w", i, err)
				return
			}
			defer s.Close()
			want := pattern(i, size)
			go func() {
				_, _ = s.Write(want)
				// hashicorp yamux Close is a half-close: it sends FIN and the stream stays readable.
				_ = s.Close()
			}()
			got, err := io.ReadAll(s)
			if err != nil {
				errs <- fmt.Errorf("read %d: %w", i, err)
				return
			}
			if !bytes.Equal(got, want) {
				errs <- fmt.Errorf("stream %d: got %d bytes sha %x, want %d", i, len(got), sha256.Sum256(got), len(want))
			}
		}(i)
	}
	wg.Wait()
	close(errs)
	for err := range errs {
		return err
	}
	return nil
}

func runClient(path string) error {
	conn, err := net.Dial("unix", path)
	if err != nil {
		return err
	}
	sess, err := yamux.Client(conn, nil)
	if err != nil {
		return err
	}
	streams, size, idle := env("STREAMS", 300), env("SIZE", 1<<16), env("IDLE", 35)
	big := env("BIG", 64<<20)
	start := time.Now()
	if err := round(sess, streams, size); err != nil {
		return err
	}
	fmt.Printf("client: %d streams x %d bytes ok in %v\n", streams, size, time.Since(start))
	start = time.Now()
	if err := round(sess, 1, big); err != nil {
		return err
	}
	fmt.Printf("client: 1 stream x %d bytes ok in %v\n", big, time.Since(start))
	fmt.Printf("client: idling %ds\n", idle)
	time.Sleep(time.Duration(idle) * time.Second)
	if err := round(sess, streams, size); err != nil {
		return fmt.Errorf("after idle: %w", err)
	}
	fmt.Println("client: after idle ok")
	return sess.Close()
}

func main() {
	var err error
	switch os.Args[1] {
	case "server":
		err = runServer(os.Args[2])
	case "client":
		err = runClient(os.Args[2])
	}
	if err != nil {
		fmt.Println("FAIL:", err)
		os.Exit(1)
	}
}
