// Package rpc is the sidecar harness: it owns stdio, ids, cancellation and panics,
// so an engine only implements Engine, Conn and Cursor.
package rpc

import (
	"context"
	"encoding/json"

	"github.com/chud-lori/datagrep/sidecar/common/wire"
)

type Info struct {
	Engine   string
	Version  string
	Language json.RawMessage
	Caps     uint32
}

type Engine interface {
	Info() Info
	Connect(ctx context.Context, p wire.ConnectParams) (Conn, wire.Server, error)
}

// A Conn is used by one request at a time per cursor, but requests on it may overlap.
type Conn interface {
	Ping(ctx context.Context) error
	// Execute returns a nil Cursor for an ack. ctx lives as long as the cursor does.
	Execute(ctx context.Context, p wire.ExecuteParams, args []any) (Cursor, wire.Shape, error)
	SetReadOnly(ctx context.Context, on bool) (string, error)
	Children(ctx context.Context, p wire.ChildrenParams) ([]wire.Node, error)
	Describe(ctx context.Context, path []string) (wire.DescribeReply, error)
	Close() error
}

// Fetch returns at most hint.MaxRows rows; done means no further rows exist.
type Cursor interface {
	Fetch(ctx context.Context, hint wire.FetchParams) (rows [][]any, done bool, err error)
	Close() error
}
