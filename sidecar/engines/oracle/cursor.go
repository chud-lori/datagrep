package main

import (
	"context"
	"database/sql"
	"fmt"
	"strconv"
	"time"

	"github.com/chud-lori/datagrep/sidecar/common/wire"
)

type column struct {
	native  string
	logical string
}

// logicalFor maps go-ora's type names onto the parent's logical types.
func logicalFor(native string, precision, scale int64, hasScale bool) string {
	switch native {
	case "NUMBER":
		if hasScale && scale == 0 && precision > 0 && precision <= 18 {
			return wire.I64
		}
		return wire.Decimal
	case "IBFloat", "IBDouble", "BFloat", "BDouble":
		return wire.F64
	case "DATE", "TIMESTAMP", "TimeStampDTY", "TIMESTAMPTZ", "TimeStampTZ_DTY",
		"TimeStampeLTZ", "TimeStampLTZ_DTY":
		return wire.Timestamp
	case "RAW", "LongRaw", "OCIBlobLocator":
		return wire.Bytes
	}
	return wire.Str
}

func zoneOf(native string) string {
	switch native {
	case "TIMESTAMPTZ", "TimeStampTZ_DTY":
		return "offset"
	case "TimeStampeLTZ", "TimeStampLTZ_DTY":
		return "utc"
	}
	return "naive"
}

type cursor struct {
	rows *sql.Rows
	cols []column
}

func newCursor(rows *sql.Rows) (*cursor, []wire.Field, error) {
	types, err := rows.ColumnTypes()
	if err != nil {
		return nil, nil, err
	}
	cols := make([]column, len(types))
	fields := make([]wire.Field, len(types))
	for i, t := range types {
		native := t.DatabaseTypeName()
		precision, scale, ok := t.DecimalSize()
		nullable, _ := t.Nullable()
		cols[i] = column{native: native, logical: logicalFor(native, precision, scale, ok)}
		fields[i] = wire.Field{Name: t.Name(), Logical: cols[i].logical, Nullable: nullable, NativeType: &cols[i].native}
	}
	return &cursor{rows: rows, cols: cols}, fields, nil
}

func (c *cursor) Fetch(ctx context.Context, hint wire.FetchParams) ([][]any, bool, error) {
	deadline := time.Now().Add(time.Duration(hint.TargetMs) * time.Millisecond)
	var out [][]any
	bytes := 0
	for len(out) < int(hint.MaxRows) {
		if len(out) > 0 && (bytes >= int(hint.MaxBytes) || (hint.TargetMs > 0 && time.Now().After(deadline))) {
			return out, false, nil
		}
		if !c.rows.Next() {
			if err := c.rows.Err(); err != nil {
				return nil, false, queryError(ctx, err)
			}
			return out, true, nil
		}
		raw := make([]any, len(c.cols))
		ptrs := make([]any, len(c.cols))
		for i := range raw {
			ptrs[i] = &raw[i]
		}
		if err := c.rows.Scan(ptrs...); err != nil {
			return nil, false, queryError(ctx, err)
		}
		row := make([]any, len(c.cols))
		for i, v := range raw {
			row[i] = encodeCell(c.cols[i], v)
			bytes += cellSize(v)
		}
		out = append(out, row)
	}
	return out, false, nil
}

func (c *cursor) Close() error { return c.rows.Close() }

func cellSize(v any) int {
	switch x := v.(type) {
	case string:
		return len(x)
	case []byte:
		return len(x)
	}
	return 8
}

func encodeCell(col column, v any) any {
	if v == nil {
		return nil
	}
	switch col.logical {
	case wire.I64:
		if s, ok := v.(string); ok {
			if n, err := strconv.ParseInt(s, 10, 64); err == nil {
				return n
			}
			return wire.Tagged("decimal", s)
		}
	case wire.Decimal:
		if s, ok := v.(string); ok {
			return s
		}
	case wire.F64:
		switch x := v.(type) {
		case float64:
			return wire.EncodeFloat(x)
		case float32:
			return wire.EncodeFloat(float64(x))
		}
	case wire.Timestamp:
		if t, ok := v.(time.Time); ok {
			switch zoneOf(col.native) {
			case "utc":
				return wire.EncodeTimestamp(t.UTC(), "utc")
			case "offset":
				_, off := t.Zone()
				return wire.EncodeTimestamp(t, wire.EncodeOffset(off))
			}
			return wire.EncodeTimestamp(t, "naive")
		}
	case wire.Bytes:
		if b, ok := v.([]byte); ok {
			return wire.EncodeBytes(b)
		}
	case wire.Str:
		switch x := v.(type) {
		case string:
			return x
		case []byte:
			return string(x)
		}
		return fmt.Sprint(v)
	}
	return wire.Tagged("str", fmt.Sprint(v))
}
