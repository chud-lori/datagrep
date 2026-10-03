// Package frame reads and writes 4-byte big-endian length-prefixed frames.
package frame

import (
	"encoding/binary"
	"errors"
	"fmt"
	"io"
)

// MaxFrame is four times the parent's default fetch max_bytes.
const MaxFrame = 16 << 20

var ErrOversized = errors.New("frame exceeds the size cap")

// Read returns io.EOF only on a clean end between frames; the cap is checked before allocating.
func Read(r io.Reader, limit uint32) ([]byte, error) {
	var header [4]byte
	if _, err := io.ReadFull(r, header[:]); err != nil {
		if errors.Is(err, io.ErrUnexpectedEOF) {
			return nil, fmt.Errorf("stream ended inside a frame header: %w", err)
		}
		return nil, err
	}
	n := binary.BigEndian.Uint32(header[:])
	if n > limit {
		return nil, fmt.Errorf("%w: %d > %d bytes", ErrOversized, n, limit)
	}
	body := make([]byte, n)
	if _, err := io.ReadFull(r, body); err != nil {
		if errors.Is(err, io.EOF) {
			err = io.ErrUnexpectedEOF
		}
		return nil, fmt.Errorf("stream ended inside a frame: %w", err)
	}
	return body, nil
}

func Write(w io.Writer, body []byte) error {
	if len(body) > MaxFrame {
		return fmt.Errorf("%w: outgoing %d bytes", ErrOversized, len(body))
	}
	buf := make([]byte, 4+len(body))
	binary.BigEndian.PutUint32(buf, uint32(len(body)))
	copy(buf[4:], body)
	_, err := w.Write(buf)
	return err
}
