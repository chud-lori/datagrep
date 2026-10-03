package rpc

import (
	"context"
	"encoding/json"
	"io"
	"strings"
	"testing"
	"time"

	"github.com/chud-lori/datagrep/sidecar/common/frame"
	"github.com/chud-lori/datagrep/sidecar/common/wire"
)

type testEngine struct{}

func (testEngine) Info() Info {
	return Info{Engine: "test", Version: "1", Language: wire.LanguageUnclassified, Caps: wire.CapServerCancel}
}

func (testEngine) Connect(_ context.Context, p wire.ConnectParams) (Conn, wire.Server, error) {
	if p.Secrets["password"].Reveal() != "hunter2" {
		return nil, wire.Server{}, wire.Errorf(wire.KindAuth, "bad password")
	}
	return testConn{}, wire.Server{Product: "test", Version: "1"}, nil
}

type testConn struct{}

func (testConn) Ping(context.Context) error { return nil }
func (testConn) SetReadOnly(context.Context, bool) (string, error) {
	return wire.EnforcementClient, nil
}
func (testConn) Children(context.Context, wire.ChildrenParams) ([]wire.Node, error) { return nil, nil }
func (testConn) Describe(context.Context, []string) (wire.DescribeReply, error) {
	return wire.DescribeReply{}, nil
}
func (testConn) Close() error { return nil }

func (testConn) Execute(ctx context.Context, p wire.ExecuteParams, _ []any) (Cursor, wire.Shape, error) {
	switch p.Text {
	case "panic":
		panic("boom")
	case "ack":
		n := uint64(2)
		return nil, wire.Ack(&n), nil
	case "sleep":
		<-ctx.Done()
		return nil, wire.Shape{}, ctx.Err()
	}
	return &testCursor{left: 3, block: p.Text == "block"}, wire.Table([]wire.Field{{Name: "n", Logical: wire.I64}}), nil
}

type testCursor struct {
	left  int
	block bool
}

func (c *testCursor) Fetch(ctx context.Context, hint wire.FetchParams) ([][]any, bool, error) {
	if c.block {
		<-ctx.Done()
		return nil, false, ctx.Err()
	}
	var rows [][]any
	for c.left > 0 && len(rows) < int(hint.MaxRows) {
		rows = append(rows, []any{c.left})
		c.left--
	}
	return rows, c.left == 0, nil
}

func (c *testCursor) Close() error { return nil }

type peer struct {
	t   *testing.T
	in  *io.PipeWriter
	out *io.PipeReader
	id  uint64
	end chan error
}

func start(t *testing.T) *peer {
	inR, inW := io.Pipe()
	outR, outW := io.Pipe()
	p := &peer{t: t, in: inW, out: outR, end: make(chan error, 1)}
	go func() { p.end <- Serve(testEngine{}, inR, outW); outW.Close() }()
	t.Cleanup(func() { inW.Close() })
	return p
}

func (p *peer) send(id *uint64, m string, params any) {
	raw, _ := json.Marshal(params)
	body, _ := json.Marshal(wire.Request{ID: id, M: m, P: raw})
	if err := frame.Write(p.in, body); err != nil {
		p.t.Fatal(err)
	}
}

func (p *peer) call(m string, params any) map[string]json.RawMessage {
	p.id++
	id := p.id
	p.send(&id, m, params)
	return p.read(id)
}

func (p *peer) read(id uint64) map[string]json.RawMessage {
	body, err := frame.Read(p.out, frame.MaxFrame)
	if err != nil {
		p.t.Fatal(err)
	}
	var r map[string]json.RawMessage
	if err := json.Unmarshal(body, &r); err != nil {
		p.t.Fatal(err)
	}
	if string(r["id"]) != jsonOf(id) {
		p.t.Fatalf("reply id %s, want %d", r["id"], id)
	}
	return r
}

func jsonOf(v any) string { b, _ := json.Marshal(v); return string(b) }

func (p *peer) open() {
	if r := p.call("hello", wire.HelloParams{Protocol: [2]uint32{1, 1}}); r["err"] != nil {
		p.t.Fatalf("hello: %s", r["err"])
	}
	r := p.call("connect", map[string]any{"conn": 1, "config": map[string]any{}, "secrets": map[string]string{"password": "hunter2"}})
	if r["err"] != nil {
		p.t.Fatalf("connect: %s", r["err"])
	}
}

func TestHelloRejectsAnUnspokenProtocol(t *testing.T) {
	p := start(t)
	r := p.call("hello", wire.HelloParams{Protocol: [2]uint32{7, 9}})
	if !strings.Contains(string(r["err"]), `"unsupported"`) {
		t.Fatalf("got %s", r["err"])
	}
}

func TestStreamsInHintSizedBatchesThenEOF(t *testing.T) {
	p := start(t)
	p.open()
	r := p.call("execute", map[string]any{"conn": 1, "text": "rows"})
	var ex wire.ExecuteReply
	_ = json.Unmarshal(r["ok"], &ex)
	if ex.Cursor == 0 || ex.Shape.Kind != "table" {
		t.Fatalf("execute: %s %s", r["ok"], r["err"])
	}
	var got []string
	for {
		r := p.call("fetch", map[string]any{"conn": 1, "cursor": ex.Cursor, "max_rows": 2})
		var f struct{ Batch *wire.Batch }
		_ = json.Unmarshal(r["ok"], &f)
		if f.Batch == nil {
			break
		}
		got = append(got, jsonOf(f.Batch.Rows))
	}
	if strings.Join(got, " ") != "[[3],[2]] [[1]]" {
		t.Fatalf("batches %v", got)
	}
	r = p.call("fetch", map[string]any{"conn": 1, "cursor": ex.Cursor, "max_rows": 2})
	if r["err"] == nil {
		t.Fatal("an exhausted cursor is released")
	}
}

func TestAckHasNoCursor(t *testing.T) {
	p := start(t)
	p.open()
	r := p.call("execute", map[string]any{"conn": 1, "text": "ack"})
	if string(r["ok"]) != `{"cursor":0,"shape":{"kind":"ack","affected":2}}` {
		t.Fatalf("got %s", r["ok"])
	}
}

func TestCancelReachesABlockedFetch(t *testing.T) {
	p := start(t)
	p.open()
	var ex wire.ExecuteReply
	_ = json.Unmarshal(p.call("execute", map[string]any{"conn": 1, "text": "block"})["ok"], &ex)
	p.id++
	id := p.id
	p.send(&id, "fetch", map[string]any{"conn": 1, "cursor": ex.Cursor, "max_rows": 2})
	time.Sleep(20 * time.Millisecond)
	p.send(nil, "cancel", map[string]any{"conn": 1})
	if r := p.read(id); !strings.Contains(string(r["err"]), `"cancelled"`) {
		t.Fatalf("got %s", r["err"])
	}
	if r := p.call("ping", map[string]any{"conn": 1}); r["err"] != nil {
		t.Fatalf("connection unusable after cancel: %s", r["err"])
	}
}

func TestStatementTimeoutIsATimeout(t *testing.T) {
	p := start(t)
	p.open()
	r := p.call("execute", map[string]any{"conn": 1, "text": "sleep", "timeout_ms": 20})
	if !strings.Contains(string(r["err"]), `"timeout"`) {
		t.Fatalf("got %s", r["err"])
	}
}

func TestAPanicIsAnErrorNotACrash(t *testing.T) {
	p := start(t)
	p.open()
	if r := p.call("execute", map[string]any{"conn": 1, "text": "panic"}); !strings.Contains(string(r["err"]), `"panic"`) {
		t.Fatalf("got %s", r["err"])
	}
	if r := p.call("ping", map[string]any{"conn": 1}); r["err"] != nil {
		t.Fatalf("sidecar died with the panic: %s", r["err"])
	}
}

func TestWrongPasswordIsAuth(t *testing.T) {
	p := start(t)
	r := p.call("connect", map[string]any{"conn": 1, "secrets": map[string]string{"password": "nope"}})
	if !strings.Contains(string(r["err"]), `"auth"`) {
		t.Fatalf("got %s", r["err"])
	}
}

func TestAnOversizedRequestStopsTheServer(t *testing.T) {
	p := start(t)
	go func() { _, _ = p.in.Write([]byte{0xff, 0xff, 0xff, 0xff}) }()
	select {
	case err := <-p.end:
		if err == nil || !strings.Contains(err.Error(), "size cap") {
			t.Fatalf("got %v", err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("server kept reading past an oversized header")
	}
}
