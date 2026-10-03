package wire

// Logical types as the parent's LogicalType names them.
const (
	Bool      = "Bool"
	I64       = "I64"
	U64       = "U64"
	F64       = "F64"
	Decimal   = "Decimal"
	Str       = "Str"
	Bytes     = "Bytes"
	Date      = "Date"
	Time      = "Time"
	Timestamp = "Timestamp"
	Interval  = "Interval"
	UUID      = "Uuid"
	JSON      = "Json"
	Array     = "Array"
	Document  = "Document"
	Unknown   = "Unknown"
)

type Field struct {
	Name       string  `json:"name"`
	Logical    string  `json:"logical"`
	Nullable   bool    `json:"nullable"`
	NativeType *string `json:"native_type,omitempty"`
}

type Shape struct {
	Kind     string  `json:"kind"`
	Fields   []Field `json:"fields,omitempty"`
	Affected *uint64 `json:"affected,omitempty"`
	Message  *string `json:"message,omitempty"`
}

func Table(fields []Field) Shape { return Shape{Kind: "table", Fields: fields} }

func Ack(affected *uint64) Shape { return Shape{Kind: "ack", Affected: affected} }
