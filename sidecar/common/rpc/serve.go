package rpc

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"os"
	"runtime/debug"
	"sync"
	"sync/atomic"
	"time"

	"github.com/chud-lori/datagrep/sidecar/common/frame"
	"github.com/chud-lori/datagrep/sidecar/common/wire"
)

type conn struct {
	impl Conn
	mu   sync.Mutex
	// Every context a cancel for this conn must reach: in-flight executes and open cursors.
	live map[*context.CancelFunc]struct{}
}

type cursor struct {
	conn   uint64
	impl   Cursor
	cancel context.CancelFunc
	mu     sync.Mutex
	ctx    context.Context
}

type server struct {
	engine  Engine
	out     io.Writer
	outMu   sync.Mutex
	mu      sync.Mutex
	conns   map[uint64]*conn
	cursors map[uint64]*cursor
	next    uint64
	wg      sync.WaitGroup
}

// Main serves stdin/stdout until the parent closes stdin. stdout carries frames only,
// so anything else that prints is pointed at stderr.
func Main(engine Engine) {
	out := os.Stdout
	os.Stdout = os.Stderr
	log.SetOutput(os.Stderr)
	if err := Serve(engine, os.Stdin, out); err != nil {
		log.Printf("sidecar: %v", err)
		os.Exit(2)
	}
}

func Serve(engine Engine, in io.Reader, out io.Writer) error {
	s := &server{engine: engine, out: out, conns: map[uint64]*conn{}, cursors: map[uint64]*cursor{}, next: 1}
	defer s.shutdown()
	for {
		body, err := frame.Read(in, frame.MaxFrame)
		if errors.Is(err, io.EOF) {
			return nil
		}
		if err != nil {
			return err
		}
		var req wire.Request
		if err := json.Unmarshal(body, &req); err != nil {
			return fmt.Errorf("unreadable request: %w", err)
		}
		if req.ID == nil {
			if req.M == "cancel" {
				s.cancel(req.P)
			}
			continue
		}
		s.wg.Add(1)
		go s.handle(*req.ID, req.M, req.P)
	}
}

func (s *server) shutdown() {
	s.mu.Lock()
	cursors, conns := s.cursors, s.conns
	s.cursors, s.conns = map[uint64]*cursor{}, map[uint64]*conn{}
	s.mu.Unlock()
	for _, c := range cursors {
		c.cancel()
	}
	for _, c := range conns {
		c.cancelAll()
	}
	done := make(chan struct{})
	go func() { s.wg.Wait(); close(done) }()
	select {
	case <-done:
	case <-time.After(time.Second):
	}
	for _, c := range cursors {
		_ = c.impl.Close()
	}
	for _, c := range conns {
		_ = c.impl.Close()
	}
}

func (s *server) reply(r wire.Reply) {
	body, err := json.Marshal(r)
	if err != nil {
		body, _ = json.Marshal(wire.Reply{ID: r.ID, Err: wire.Errorf(wire.KindPanic, "encoding reply: %v", err)})
	}
	s.outMu.Lock()
	defer s.outMu.Unlock()
	if err := frame.Write(s.out, body); err != nil {
		log.Printf("sidecar: writing reply %d: %v", r.ID, err)
		os.Exit(2)
	}
}

func (s *server) handle(id uint64, method string, params json.RawMessage) {
	defer s.wg.Done()
	defer func() {
		if p := recover(); p != nil {
			log.Printf("sidecar: panic in %s: %v\n%s", method, p, debug.Stack())
			s.reply(wire.Reply{ID: id, Err: wire.Errorf(wire.KindPanic, "%s: %v", method, p)})
		}
	}()
	ok, err := s.dispatch(method, params)
	if err != nil {
		s.reply(wire.Reply{ID: id, Err: toWire(err)})
		return
	}
	if ok == nil {
		ok = struct{}{}
	}
	s.reply(wire.Reply{ID: id, OK: ok})
}

func toWire(err error) *wire.Error {
	var we *wire.Error
	switch {
	case errors.As(err, &we):
		return we
	case errors.Is(err, context.Canceled):
		return wire.Errorf(wire.KindCancelled, "%v", err)
	case errors.Is(err, context.DeadlineExceeded):
		return wire.Errorf(wire.KindTimeout, "%v", err)
	}
	return wire.Errorf(wire.KindQuery, "%v", err)
}

func decode[T any](raw json.RawMessage) (T, error) {
	var v T
	if err := json.Unmarshal(raw, &v); err != nil {
		return v, wire.Errorf(wire.KindQuery, "bad params: %v", err)
	}
	return v, nil
}

func (s *server) dispatch(method string, raw json.RawMessage) (any, error) {
	switch method {
	case "hello":
		return s.hello(raw)
	case "connect":
		return s.connect(raw)
	case "execute":
		return s.execute(raw)
	case "fetch":
		return s.fetch(raw)
	case "close_cursor":
		p, err := decode[wire.CursorParams](raw)
		if err != nil {
			return nil, err
		}
		return nil, s.closeCursor(p.Cursor)
	}
	p, err := decode[wire.ConnParams](raw)
	if err != nil {
		return nil, err
	}
	c, err := s.conn(p.Conn)
	if err != nil {
		return nil, err
	}
	ctx, done := c.track(context.Background())
	defer done()
	switch method {
	case "ping":
		return nil, c.impl.Ping(ctx)
	case "set_read_only":
		p, err := decode[wire.ReadOnlyParams](raw)
		if err != nil {
			return nil, err
		}
		e, err := c.impl.SetReadOnly(ctx, p.On)
		return wire.ReadOnlyReply{Enforcement: e}, err
	case "children":
		p, err := decode[wire.ChildrenParams](raw)
		if err != nil {
			return nil, err
		}
		items, err := c.impl.Children(ctx, p)
		if items == nil {
			items = []wire.Node{}
		}
		return wire.ChildrenReply{Items: items}, err
	case "describe":
		p, err := decode[wire.DescribeParams](raw)
		if err != nil {
			return nil, err
		}
		return c.impl.Describe(ctx, p.Path)
	case "disconnect":
		return nil, s.disconnect(p.Conn)
	}
	return nil, wire.Errorf(wire.KindUnsupported, "method %q", method)
}

func (s *server) hello(raw json.RawMessage) (any, error) {
	p, err := decode[wire.HelloParams](raw)
	if err != nil {
		return nil, err
	}
	if p.Protocol[0] > wire.ProtocolMax || p.Protocol[1] < wire.ProtocolMin {
		return nil, wire.Errorf(wire.KindUnsupported, "protocol %v, this sidecar speaks %d..%d",
			p.Protocol, wire.ProtocolMin, wire.ProtocolMax)
	}
	info := s.engine.Info()
	return wire.HelloReply{
		Protocol:      min(p.Protocol[1], wire.ProtocolMax),
		Engine:        info.Engine,
		EngineVersion: info.Version,
		Language:      info.Language,
		Caps:          info.Caps,
	}, nil
}

func (s *server) connect(raw json.RawMessage) (any, error) {
	p, err := decode[wire.ConnectParams](raw)
	if err != nil {
		return nil, err
	}
	ctx := context.Background()
	if p.ConnectTimeoutMs != nil {
		var cancel context.CancelFunc
		ctx, cancel = context.WithTimeout(ctx, time.Duration(*p.ConnectTimeoutMs)*time.Millisecond)
		defer cancel()
	}
	impl, server, err := s.engine.Connect(ctx, p)
	if err != nil {
		return nil, err
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, taken := s.conns[p.Conn]; taken {
		_ = impl.Close()
		return nil, wire.Errorf(wire.KindConnect, "connection %d already exists", p.Conn)
	}
	s.conns[p.Conn] = &conn{impl: impl, live: map[*context.CancelFunc]struct{}{}}
	return wire.ConnectReply{Server: server}, nil
}

func (s *server) conn(id uint64) (*conn, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	c, ok := s.conns[id]
	if !ok {
		return nil, wire.Errorf(wire.KindQuery, "no connection %d", id)
	}
	return c, nil
}

// track derives a context that a cancel for this conn reaches; done releases it.
func (c *conn) track(parent context.Context) (context.Context, func()) {
	ctx, cancel := context.WithCancel(parent)
	c.mu.Lock()
	c.live[&cancel] = struct{}{}
	c.mu.Unlock()
	return ctx, func() {
		c.mu.Lock()
		delete(c.live, &cancel)
		c.mu.Unlock()
		cancel()
	}
}

func (c *conn) cancelAll() {
	c.mu.Lock()
	defer c.mu.Unlock()
	for cancel := range c.live {
		(*cancel)()
	}
}

func (s *server) cancel(raw json.RawMessage) {
	p, err := decode[wire.ConnParams](raw)
	if err != nil {
		return
	}
	if c, err := s.conn(p.Conn); err == nil {
		c.cancelAll()
	}
}

func (s *server) execute(raw json.RawMessage) (any, error) {
	p, err := decode[wire.ExecuteParams](raw)
	if err != nil {
		return nil, err
	}
	c, err := s.conn(p.Conn)
	if err != nil {
		return nil, err
	}
	args := make([]any, len(p.Params))
	for i, raw := range p.Params {
		if args[i], err = wire.DecodeParam(raw); err != nil {
			return nil, err
		}
	}
	ctx, done := c.track(context.Background())
	// The statement timeout covers execute only; the cursor keeps ctx afterwards.
	var timedOut atomic.Bool
	if p.TimeoutMs != nil {
		timer := time.AfterFunc(time.Duration(*p.TimeoutMs)*time.Millisecond, func() {
			timedOut.Store(true)
			done()
		})
		defer timer.Stop()
	}
	impl, shape, err := c.impl.Execute(ctx, p, args)
	if ctx.Err() != nil && err == nil && impl != nil {
		_ = impl.Close()
		err = ctx.Err()
	}
	if err != nil || impl == nil {
		done()
		if timedOut.Load() {
			err = wire.Errorf(wire.KindTimeout, "statement exceeded %d ms", *p.TimeoutMs)
		}
		return wire.ExecuteReply{Shape: shape}, err
	}
	s.mu.Lock()
	id := s.next
	s.next++
	s.cursors[id] = &cursor{conn: p.Conn, impl: impl, cancel: done, ctx: ctx}
	s.mu.Unlock()
	return wire.ExecuteReply{Cursor: id, Shape: shape}, nil
}

func (s *server) fetch(raw json.RawMessage) (any, error) {
	p, err := decode[wire.FetchParams](raw)
	if err != nil {
		return nil, err
	}
	s.mu.Lock()
	cur, ok := s.cursors[p.Cursor]
	s.mu.Unlock()
	if !ok {
		return nil, wire.Errorf(wire.KindQuery, "no cursor %d", p.Cursor)
	}
	cur.mu.Lock()
	defer cur.mu.Unlock()
	if err := cur.ctx.Err(); err != nil {
		return nil, wire.Errorf(wire.KindCancelled, "cursor %d was cancelled", p.Cursor)
	}
	hint := p
	hint.MaxRows = max(hint.MaxRows, 1)
	rows, done, err := cur.impl.Fetch(cur.ctx, hint)
	if err != nil {
		if cur.ctx.Err() != nil {
			err = wire.Errorf(wire.KindCancelled, "%v", err)
		}
		return nil, err
	}
	if len(rows) > int(hint.MaxRows) {
		return nil, wire.Errorf(wire.KindPanic, "engine returned %d rows for a %d-row fetch", len(rows), hint.MaxRows)
	}
	if len(rows) == 0 && done {
		s.forget(p.Cursor)
		_ = cur.impl.Close()
		cur.cancel()
		return wire.FetchReply{}, nil
	}
	if rows == nil {
		rows = [][]any{}
	}
	return wire.FetchReply{Batch: &wire.Batch{Rows: rows}}, nil
}

func (s *server) forget(id uint64) *cursor {
	s.mu.Lock()
	defer s.mu.Unlock()
	cur := s.cursors[id]
	delete(s.cursors, id)
	return cur
}

// closeCursor cancels first so a fetch blocked in the driver lets go of the cursor lock.
func (s *server) closeCursor(id uint64) error {
	cur := s.forget(id)
	if cur == nil {
		return nil
	}
	cur.cancel()
	cur.mu.Lock()
	defer cur.mu.Unlock()
	return cur.impl.Close()
}

func (s *server) disconnect(id uint64) error {
	s.mu.Lock()
	c := s.conns[id]
	delete(s.conns, id)
	var mine []uint64
	for cid, cur := range s.cursors {
		if cur.conn == id {
			mine = append(mine, cid)
		}
	}
	s.mu.Unlock()
	for _, cid := range mine {
		_ = s.closeCursor(cid)
	}
	if c == nil {
		return nil
	}
	c.cancelAll()
	return c.impl.Close()
}
