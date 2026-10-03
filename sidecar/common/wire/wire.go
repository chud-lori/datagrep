// Package wire mirrors the parent's protocol types: envelopes, shapes, cells and error kinds.
package wire

import (
	"encoding/json"

	"github.com/chud-lori/datagrep/sidecar/common/secret"
)

const (
	ProtocolMin = 1
	ProtocolMax = 1
)

// Caps bits, identical to the parent's Caps flags.
const (
	CapTransactions     = 1 << 0
	CapDDL              = 1 << 1
	CapExplain          = 1 << 2
	CapServerCancel     = 1 << 4
	CapSchemaDeclared   = 1 << 7
	CapReadOnlySession  = 1 << 9
	CapPositionalParams = 1 << 13
)

// Language values as the parent serializes its LanguageId.
var LanguageUnclassified = json.RawMessage(`"Unclassified"`)

// Request has no ID only for "cancel", which is never answered.
type Request struct {
	ID *uint64         `json:"id,omitempty"`
	M  string          `json:"m"`
	P  json.RawMessage `json:"p"`
}

type Reply struct {
	ID  uint64 `json:"id"`
	OK  any    `json:"ok,omitempty"`
	Err *Error `json:"err,omitempty"`
}

type HelloParams struct {
	Protocol   [2]uint32 `json:"protocol"`
	AppVersion string    `json:"app_version"`
	Codec      string    `json:"codec"`
}

type HelloReply struct {
	Protocol      uint32          `json:"protocol"`
	Engine        string          `json:"engine"`
	EngineVersion string          `json:"engine_version"`
	Language      json.RawMessage `json:"language"`
	Caps          uint32          `json:"caps"`
}

type ConnectParams struct {
	Conn             uint64                   `json:"conn"`
	Config           map[string]any           `json:"config"`
	Secrets          map[string]secret.Secret `json:"secrets"`
	ConnectTimeoutMs *uint64                  `json:"connect_timeout_ms"`
	ApplicationName  *string                  `json:"application_name"`
}

type Server struct {
	Product string      `json:"product"`
	Version string      `json:"version"`
	Details [][2]string `json:"details"`
}

type ConnectReply struct {
	Server Server `json:"server"`
}

type ConnParams struct {
	Conn uint64 `json:"conn"`
}

type ExecuteParams struct {
	Conn           uint64            `json:"conn"`
	Text           string            `json:"text"`
	Params         []json.RawMessage `json:"params"`
	TimeoutMs      *uint64           `json:"timeout_ms"`
	RowLimit       *uint64           `json:"row_limit"`
	ReadOnlyAssert bool              `json:"read_only_assert"`
}

// ExecuteReply carries cursor 0 for an ack, which has no rows to fetch.
type ExecuteReply struct {
	Cursor uint64 `json:"cursor"`
	Shape  Shape  `json:"shape"`
}

type FetchParams struct {
	Conn     uint64 `json:"conn"`
	Cursor   uint64 `json:"cursor"`
	MaxRows  uint32 `json:"max_rows"`
	MaxBytes uint32 `json:"max_bytes"`
	TargetMs uint32 `json:"target_ms"`
}

// FetchReply's nil Batch is end of stream.
type FetchReply struct {
	Batch *Batch `json:"batch"`
}

type Batch struct {
	Rows [][]any `json:"rows"`
}

type CursorParams struct {
	Conn   uint64 `json:"conn"`
	Cursor uint64 `json:"cursor"`
}

type ReadOnlyParams struct {
	Conn uint64 `json:"conn"`
	On   bool   `json:"on"`
}

const (
	EnforcementServer = "server"
	EnforcementClient = "client"
	EnforcementNone   = "none"
)

type ReadOnlyReply struct {
	Enforcement string `json:"enforcement"`
}

type ChildrenParams struct {
	Conn   uint64   `json:"conn"`
	Path   []string `json:"path"`
	Prefix *string  `json:"prefix"`
	Limit  uint32   `json:"limit"`
}

type Node struct {
	Path        []string `json:"path"`
	Kind        string   `json:"kind"`
	HasChildren bool     `json:"has_children"`
	Comment     *string  `json:"comment,omitempty"`
}

type ChildrenReply struct {
	Items []Node `json:"items"`
}

type DescribeParams struct {
	Conn uint64   `json:"conn"`
	Path []string `json:"path"`
}

type DescribeReply struct {
	Node   Node        `json:"node"`
	Fields []Field     `json:"fields,omitempty"`
	Extra  [][2]string `json:"extra"`
}

// Object kinds as the parent's ObjectKind names them.
const (
	KindSchema = "Schema"
	KindTable  = "Table"
	KindView   = "View"
	KindColumn = "Column"
)
