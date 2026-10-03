package wire

import (
	"encoding/base64"
	"encoding/json"
	"fmt"
	"math"
	"time"
)

// Cells in a table row are untagged and read against the column's declared logical type.
// A cell that does not match it, and every parameter, uses Tagged.

type tagged struct {
	T string `json:"$t"`
	V any    `json:"v"`
}

func Tagged(tag string, v any) any { return tagged{T: tag, V: v} }

func EncodeBytes(b []byte) string { return base64.StdEncoding.EncodeToString(b) }

// EncodeFloat spells the values JSON cannot carry.
func EncodeFloat(f float64) any {
	switch {
	case math.IsNaN(f):
		return "NaN"
	case math.IsInf(f, 1):
		return "Infinity"
	case math.IsInf(f, -1):
		return "-Infinity"
	}
	return f
}

type timestamp struct {
	US int64  `json:"us"`
	TZ string `json:"tz"`
}

// EncodeTimestamp: tz is "utc", "naive", "+hh:mm" or an IANA name; naive reads the wall clock.
func EncodeTimestamp(t time.Time, tz string) any {
	if tz == "naive" {
		t = time.Date(t.Year(), t.Month(), t.Day(), t.Hour(), t.Minute(), t.Second(), t.Nanosecond(), time.UTC)
	}
	return timestamp{US: t.UnixMicro(), TZ: tz}
}

// EncodeOffset formats a zone offset in seconds the way EncodeTimestamp expects.
func EncodeOffset(seconds int) string {
	sign := '+'
	if seconds < 0 {
		sign, seconds = '-', -seconds
	}
	return fmt.Sprintf("%c%02d:%02d", sign, seconds/3600, seconds/60%60)
}

func EncodeDate(t time.Time) int64 {
	d := time.Date(t.Year(), t.Month(), t.Day(), 0, 0, 0, 0, time.UTC)
	return d.Unix() / 86400
}

// DecodeParam turns a tagged parameter into a database/sql argument.
func DecodeParam(raw json.RawMessage) (any, error) {
	if string(raw) == "null" {
		return nil, nil
	}
	var t struct {
		T string          `json:"$t"`
		V json.RawMessage `json:"v"`
	}
	if err := json.Unmarshal(raw, &t); err != nil || t.T == "" {
		return nil, Errorf(KindQuery, "parameter is not a tagged value: %s", raw)
	}
	var err error
	switch t.T {
	case "bool":
		var v bool
		err = json.Unmarshal(t.V, &v)
		return v, err
	case "i64":
		var v int64
		err = json.Unmarshal(t.V, &v)
		return v, err
	case "u64":
		var v uint64
		err = json.Unmarshal(t.V, &v)
		return v, err
	case "f64":
		var v any
		if err = json.Unmarshal(t.V, &v); err != nil {
			return nil, err
		}
		switch x := v.(type) {
		case float64:
			return x, nil
		case string:
			switch x {
			case "NaN":
				return math.NaN(), nil
			case "Infinity":
				return math.Inf(1), nil
			case "-Infinity":
				return math.Inf(-1), nil
			}
		}
		return nil, Errorf(KindQuery, "bad f64 parameter %s", t.V)
	case "str", "decimal", "json", "uuid":
		var v string
		err = json.Unmarshal(t.V, &v)
		return v, err
	case "bytes":
		var v string
		if err = json.Unmarshal(t.V, &v); err != nil {
			return nil, err
		}
		return base64.StdEncoding.DecodeString(v)
	case "date":
		var v int64
		err = json.Unmarshal(t.V, &v)
		return time.Unix(v*86400, 0).UTC(), err
	case "timestamp":
		var v timestamp
		if err = json.Unmarshal(t.V, &v); err != nil {
			return nil, err
		}
		return time.UnixMicro(v.US).UTC(), nil
	}
	return nil, Errorf(KindUnsupported, "parameters of type %q", t.T)
}
