package frame

import (
	"bytes"
	"errors"
	"io"
	"testing"
)

func TestRoundTripAndCleanEOF(t *testing.T) {
	var buf bytes.Buffer
	for _, body := range []string{`{"id":1}`, ""} {
		if err := Write(&buf, []byte(body)); err != nil {
			t.Fatal(err)
		}
	}
	for _, want := range []string{`{"id":1}`, ""} {
		got, err := Read(&buf, MaxFrame)
		if err != nil || string(got) != want {
			t.Fatalf("got %q, %v; want %q", got, err, want)
		}
	}
	if _, err := Read(&buf, MaxFrame); err != io.EOF {
		t.Fatalf("want io.EOF, got %v", err)
	}
}

func TestOversizedHeaderIsRejectedBeforeTheBody(t *testing.T) {
	_, err := Read(bytes.NewReader([]byte{0xff, 0xff, 0xff, 0xff}), MaxFrame)
	if !errors.Is(err, ErrOversized) {
		t.Fatalf("want ErrOversized, got %v", err)
	}
}

func TestTruncationIsNotEOF(t *testing.T) {
	for _, in := range [][]byte{{0, 0}, {0, 0, 0, 5, '{'}} {
		_, err := Read(bytes.NewReader(in), MaxFrame)
		if err == nil || err == io.EOF {
			t.Fatalf("%v: want a truncation error, got %v", in, err)
		}
	}
}
