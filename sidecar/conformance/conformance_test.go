// Package conformance replays the fixtures the Rust side also reads, so both peers agree on the bytes.
package conformance

import (
	"encoding/json"
	"math"
	"os"
	"reflect"
	"testing"
	"time"

	"github.com/chud-lori/datagrep/sidecar/common/wire"
)

func same(t *testing.T, name string, got any, want json.RawMessage) {
	t.Helper()
	b, err := json.Marshal(got)
	if err != nil {
		t.Fatal(err)
	}
	var g, w any
	_ = json.Unmarshal(b, &g)
	_ = json.Unmarshal(want, &w)
	if !reflect.DeepEqual(g, w) {
		t.Errorf("%s: Go encodes %s, fixture says %s", name, b, want)
	}
}

func TestCells(t *testing.T) {
	jan1 := time.Date(2024, 1, 1, 0, 0, 0, 123456000, time.UTC)
	encoded := map[string]any{
		"i64":              int64(math.MinInt64),
		"decimal":          "12345678901234567890.10",
		"bytes":            wire.EncodeBytes([]byte{0, 1, 255}),
		"f64_nan":          wire.EncodeFloat(math.NaN()),
		"f64_neg_inf":      wire.EncodeFloat(math.Inf(-1)),
		"date":             wire.EncodeDate(jan1),
		"timestamp_utc":    wire.EncodeTimestamp(jan1, "utc"),
		"timestamp_naive":  wire.EncodeTimestamp(time.Date(2024, 1, 1, 0, 0, 0, 0, time.FixedZone("x", 3600)), "naive"),
		"timestamp_offset": wire.EncodeTimestamp(time.Unix(0, 0), wire.EncodeOffset(-(9*3600 + 30*60))),
		"null":             nil,
		"tagged_override":  wire.Tagged("str", "n/a"),
	}
	raw, err := os.ReadFile("cells.json")
	if err != nil {
		t.Fatal(err)
	}
	var cases []struct {
		Name string
		Wire json.RawMessage
	}
	if err := json.Unmarshal(raw, &cases); err != nil {
		t.Fatal(err)
	}
	for _, c := range cases {
		v, ok := encoded[c.Name]
		if !ok {
			t.Errorf("%s: no Go encoding for this fixture", c.Name)
			continue
		}
		same(t, c.Name, v, c.Wire)
	}
}

func TestFrames(t *testing.T) {
	raw, err := os.ReadFile("frames.json")
	if err != nil {
		t.Fatal(err)
	}
	var fixtures map[string]json.RawMessage
	if err := json.Unmarshal(raw, &fixtures); err != nil {
		t.Fatal(err)
	}
	str := func(s string) *string { return &s }
	three := uint64(3)
	pos := uint32(14)
	encoded := map[string]any{
		"hello": wire.HelloReply{Protocol: 1, Engine: "oracle", EngineVersion: "0.4.0",
			Language: wire.LanguageUnclassified, Caps: wire.CapServerCancel | wire.CapSchemaDeclared | wire.CapPositionalParams},
		"execute_table": wire.ExecuteReply{Cursor: 7, Shape: wire.Table([]wire.Field{
			{Name: "ID", Logical: wire.Decimal, NativeType: str("NUMBER")},
			{Name: "NAME", Logical: wire.Str, Nullable: true},
		})},
		"execute_ack": wire.ExecuteReply{Shape: wire.Ack(&three)},
		"fetch_rows":  wire.FetchReply{Batch: &wire.Batch{Rows: [][]any{{"1", "SCOTT"}, {"2", nil}}}},
		"fetch_eof":   wire.FetchReply{},
		"error": wire.Reply{ID: 4, Err: &wire.Error{Kind: wire.KindQuery, Code: str("ORA-00942"),
			Message: "table or view does not exist", Position: &pos}},
	}
	for name, want := range fixtures {
		v, ok := encoded[name]
		if !ok {
			t.Errorf("%s: no Go encoding for this fixture", name)
			continue
		}
		same(t, name, v, want)
	}
}
