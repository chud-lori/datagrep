package wire

import "fmt"

// Error kinds the parent maps onto its DbError; "safety" is never valid from a sidecar.
const (
	KindConnect     = "connect"
	KindAuth        = "auth"
	KindTLS         = "tls"
	KindQuery       = "query"
	KindConflict    = "conflict"
	KindTimeout     = "timeout"
	KindCancelled   = "cancelled"
	KindUnsupported = "unsupported"
	KindResource    = "resource"
	KindConfig      = "config"
	KindPanic       = "panic"
)

type Error struct {
	Kind     string  `json:"kind"`
	Code     *string `json:"code,omitempty"`
	Message  string  `json:"message"`
	Position *uint32 `json:"position,omitempty"`
}

func (e *Error) Error() string { return e.Kind + ": " + e.Message }

func Errorf(kind, format string, args ...any) *Error {
	return &Error{Kind: kind, Message: fmt.Sprintf(format, args...)}
}

func (e *Error) WithCode(code string) *Error {
	e.Code = &code
	return e
}
