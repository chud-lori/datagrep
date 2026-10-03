package main

import (
	"context"
	"encoding/json"
	"errors"
	"net"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/sijms/go-ora/v2/network"

	"github.com/chud-lori/datagrep/sidecar/common/secret"
	"github.com/chud-lori/datagrep/sidecar/common/wire"
)

func TestStatementRouting(t *testing.T) {
	for in, want := range map[string]string{
		"select 1 from dual":                             "SELECT",
		"  (SELECT 1 FROM dual)":                         "SELECT",
		"WITH x AS (SELECT 1 FROM dual) SELECT * FROM x": "WITH",
		"update t set x = 1":                             "UPDATE",
		"BEGIN NULL; END;":                               "BEGIN",
	} {
		if got := leadingKeyword(in); got != want {
			t.Errorf("%q: got %q, want %q", in, got, want)
		}
	}
	for in, want := range map[string]string{
		"SELECT 1 FROM dual; ": "SELECT 1 FROM dual",
		"BEGIN NULL; END;":     "BEGIN NULL; END;",
		"begin null; end ;":    "begin null; end ;",
		"DELETE FROM t":        "DELETE FROM t",
	} {
		if got := trimStatement(in); got != want {
			t.Errorf("%q: got %q, want %q", in, got, want)
		}
	}
}

func TestColumnTypesMapOntoLogicalTypes(t *testing.T) {
	cases := []struct {
		native           string
		precision, scale int64
		hasScale         bool
		want             string
	}{
		{"NUMBER", 10, 0, true, wire.I64},
		{"NUMBER", 19, 0, true, wire.Decimal},
		{"NUMBER", 10, 2, true, wire.Decimal},
		{"NUMBER", 0, 255, true, wire.Decimal},
		{"IBDouble", 0, 0, false, wire.F64},
		{"DATE", 0, 0, false, wire.Timestamp},
		{"TimeStampTZ_DTY", 0, 0, false, wire.Timestamp},
		{"RAW", 0, 0, false, wire.Bytes},
		{"OCIBlobLocator", 0, 0, false, wire.Bytes},
		{"NCHAR", 0, 0, false, wire.Str},
		{"OCIClobLocator", 0, 0, false, wire.Str},
		{"IntervalDS_DTY", 0, 0, false, wire.Str},
	}
	for _, c := range cases {
		if got := logicalFor(c.native, c.precision, c.scale, c.hasScale); got != c.want {
			t.Errorf("%s(%d,%d): got %s, want %s", c.native, c.precision, c.scale, got, c.want)
		}
	}
	for in, want := range map[string]string{
		"TIMESTAMP(6) WITH LOCAL TIME ZONE": "TimeStampeLTZ",
		"TIMESTAMP(6) WITH TIME ZONE":       "TIMESTAMPTZ",
		"TIMESTAMP(9)":                      "TIMESTAMP",
		"BLOB":                              "RAW",
		"VARCHAR2":                          "VARCHAR2",
	} {
		if got := dictionaryType(in); got != want {
			t.Errorf("%q: got %q, want %q", in, got, want)
		}
	}
}

func encoded(col column, v any) string {
	b, _ := json.Marshal(encodeCell(col, v))
	return string(b)
}

func TestCellsEncodeForTheirDeclaredType(t *testing.T) {
	jakarta := time.FixedZone("WIB", 7*3600)
	at := time.Date(2024, 1, 1, 12, 0, 0, 0, jakarta)
	for _, c := range []struct {
		col  column
		v    any
		want string
	}{
		{column{"NUMBER", wire.I64}, "42", `42`},
		{column{"NUMBER", wire.I64}, "1e40", `{"$t":"decimal","v":"1e40"}`},
		{column{"NUMBER", wire.Decimal}, "3.10", `"3.10"`},
		{column{"IBDouble", wire.F64}, float64(1.5), `1.5`},
		{column{"RAW", wire.Bytes}, []byte{0, 1, 255}, `"AAH/"`},
		{column{"DATE", wire.Timestamp}, at, `{"us":1704110400000000,"tz":"naive"}`},
		{column{"TIMESTAMPTZ", wire.Timestamp}, at, `{"us":1704085200000000,"tz":"+07:00"}`},
		{column{"TimeStampeLTZ", wire.Timestamp}, at, `{"us":1704085200000000,"tz":"utc"}`},
		{column{"NCHAR", wire.Str}, "x", `"x"`},
		{column{"NCHAR", wire.Str}, nil, `null`},
		{column{"NUMBER", wire.Decimal}, 7, `{"$t":"str","v":"7"}`},
	} {
		if got := encoded(c.col, c.v); got != c.want {
			t.Errorf("%s %v: got %s, want %s", c.col.native, c.v, got, c.want)
		}
	}
}

func TestOracleErrorsMapToKinds(t *testing.T) {
	auth := connectError(&network.OracleError{ErrCode: 1017, ErrMsg: "ORA-01017: invalid credential"})
	var we *wire.Error
	if !errors.As(auth, &we) || we.Kind != wire.KindAuth || *we.Code != "ORA-01017" {
		t.Fatalf("got %#v", auth)
	}
	q := queryError(context.Background(), &network.OracleError{ErrCode: 942, ErrMsg: "ORA-00942: table or view does not exist"})
	if !errors.As(q, &we) || we.Kind != wire.KindQuery || *we.Code != "ORA-00942" {
		t.Fatalf("got %#v", q)
	}
	c := queryError(context.Background(), &network.OracleError{ErrCode: 1013, ErrMsg: "ORA-01013: user requested cancel"})
	if !errors.As(c, &we) || we.Kind != wire.KindCancelled {
		t.Fatalf("got %#v", c)
	}
}

func TestAnUnreachableServerFailsWithoutShowingThePassword(t *testing.T) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	port := ln.Addr().(*net.TCPAddr).Port
	ln.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	_, _, err = engine{}.Connect(ctx, wire.ConnectParams{
		Config:  map[string]any{"host": "127.0.0.1", "port": float64(port), "user": "scott", "service": "FREEPDB1"},
		Secrets: map[string]secret.Secret{"password": secret.New("tiger-secret-77")},
	})
	if err == nil {
		t.Fatal("connected to nothing")
	}
	if strings.Contains(err.Error(), "tiger-secret-77") {
		t.Fatalf("password in error: %v", err)
	}
}

// Live check against a real server: DATAGREP_ORACLE="host:port/service user password".
func TestLiveRoundTrip(t *testing.T) {
	spec := strings.Fields(os.Getenv("DATAGREP_ORACLE"))
	if len(spec) != 3 {
		t.Skip("DATAGREP_ORACLE not set")
	}
	hostPort, service, _ := strings.Cut(spec[0], "/")
	host, portText, _ := net.SplitHostPort(hostPort)
	port, _ := net.LookupPort("tcp", portText)
	ctx := context.Background()
	c, server, err := engine{}.Connect(ctx, wire.ConnectParams{
		Config:  map[string]any{"host": host, "port": float64(port), "user": spec[1], "service": service},
		Secrets: map[string]secret.Secret{"password": secret.New(spec[2])},
	})
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	t.Logf("server: %s", server.Version)
	cur, shape, err := c.Execute(ctx, wire.ExecuteParams{Text: "SELECT level AS n FROM dual CONNECT BY level <= 1000"}, nil)
	if err != nil {
		t.Fatal(err)
	}
	t.Logf("shape: %+v", shape.Fields)
	total := 0
	for {
		rows, done, err := cur.Fetch(ctx, wire.FetchParams{MaxRows: 300, MaxBytes: 1 << 20})
		if err != nil {
			t.Fatal(err)
		}
		total += len(rows)
		if done {
			break
		}
	}
	if total != 1000 {
		t.Fatalf("got %d rows", total)
	}
}
