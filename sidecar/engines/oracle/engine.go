package main

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"strings"

	go_ora "github.com/sijms/go-ora/v2"
	"github.com/sijms/go-ora/v2/network"

	"github.com/chud-lori/datagrep/sidecar/common/rpc"
	"github.com/chud-lori/datagrep/sidecar/common/wire"
)

const version = "0.4.0"

type engine struct{}

func (engine) Info() rpc.Info {
	return rpc.Info{
		Engine:   "oracle",
		Version:  version,
		Language: wire.LanguageUnclassified,
		// go-ora sends a protocol break when a query's context is cancelled.
		Caps: wire.CapServerCancel | wire.CapSchemaDeclared | wire.CapPositionalParams,
	}
}

func str(cfg map[string]any, key string) string {
	s, _ := cfg[key].(string)
	return s
}

func (engine) Connect(ctx context.Context, p wire.ConnectParams) (rpc.Conn, wire.Server, error) {
	host, user, service := str(p.Config, "host"), str(p.Config, "user"), str(p.Config, "service")
	port := 1521
	if n, ok := p.Config["port"].(float64); ok && n > 0 {
		port = int(n)
	}
	if host == "" || user == "" || service == "" {
		return nil, wire.Server{}, wire.Errorf(wire.KindConfig, "host, user and service are required")
	}
	options := map[string]string{}
	if p.ApplicationName != nil {
		options["PROGRAM"] = *p.ApplicationName
	}
	url := go_ora.BuildUrl(host, port, service, user, p.Secrets["password"].Reveal(), options)
	db, err := sql.Open("oracle", url)
	if err != nil {
		return nil, wire.Server{}, wire.Errorf(wire.KindConfig, "%v", err)
	}
	db.SetMaxOpenConns(1)
	conn, err := db.Conn(ctx)
	if err == nil {
		err = conn.PingContext(ctx)
	}
	if err != nil {
		db.Close()
		return nil, wire.Server{}, connectError(err)
	}
	server := wire.Server{Product: "Oracle Database", Version: "unknown"}
	var banner string
	if conn.QueryRowContext(ctx, "SELECT banner FROM v$version WHERE ROWNUM = 1").Scan(&banner) == nil {
		server.Version = banner
	}
	return &oraConn{db: db, conn: conn}, server, nil
}

// The DSN carries the password, so driver errors are reduced to their Oracle code and text.
func connectError(err error) error {
	var ora *network.OracleError
	if errors.As(err, &ora) {
		kind := wire.KindConnect
		if ora.ErrCode == 1017 || ora.ErrCode == 28000 {
			kind = wire.KindAuth
		}
		return wire.Errorf(kind, "%s", ora.ErrMsg).WithCode(fmt.Sprintf("ORA-%05d", ora.ErrCode))
	}
	if errors.Is(err, context.DeadlineExceeded) {
		return wire.Errorf(wire.KindTimeout, "connect timed out")
	}
	return wire.Errorf(wire.KindConnect, "%v", err)
}

func queryError(ctx context.Context, err error) error {
	var ora *network.OracleError
	if errors.As(err, &ora) {
		if ora.ErrCode == 1013 || ctx.Err() != nil {
			return wire.Errorf(wire.KindCancelled, "%s", ora.ErrMsg)
		}
		return wire.Errorf(wire.KindQuery, "%s", ora.ErrMsg).WithCode(fmt.Sprintf("ORA-%05d", ora.ErrCode))
	}
	if ctx.Err() != nil {
		return ctx.Err()
	}
	return err
}

type oraConn struct {
	db       *sql.DB
	conn     *sql.Conn
	readOnly bool
}

func (c *oraConn) Ping(ctx context.Context) error {
	return queryError(ctx, c.conn.PingContext(ctx))
}

func (c *oraConn) Close() error {
	c.conn.Close()
	return c.db.Close()
}

// Oracle has no session-wide read-only switch, so the sidecar refuses non-queries itself.
func (c *oraConn) SetReadOnly(_ context.Context, on bool) (string, error) {
	c.readOnly = on
	return wire.EnforcementClient, nil
}

func leadingKeyword(text string) string {
	t := strings.TrimLeft(text, " \t\r\n(")
	end := strings.IndexFunc(t, func(r rune) bool {
		return !(r >= 'a' && r <= 'z' || r >= 'A' && r <= 'Z')
	})
	if end < 0 {
		end = len(t)
	}
	return strings.ToUpper(t[:end])
}

// SQL rejects a trailing semicolon that PL/SQL requires.
func trimStatement(text string) string {
	t := strings.TrimSpace(text)
	if strings.HasSuffix(t, ";") && !strings.HasSuffix(strings.ToUpper(strings.TrimRight(t[:len(t)-1], " \t\r\n")), "END") {
		t = strings.TrimRight(t[:len(t)-1], " \t\r\n")
	}
	return t
}

func (c *oraConn) Execute(ctx context.Context, p wire.ExecuteParams, args []any) (rpc.Cursor, wire.Shape, error) {
	text := trimStatement(p.Text)
	switch leadingKeyword(text) {
	case "SELECT", "WITH":
		rows, err := c.conn.QueryContext(ctx, text, args...)
		if err != nil {
			return nil, wire.Shape{}, queryError(ctx, err)
		}
		cur, fields, err := newCursor(rows)
		if err != nil {
			rows.Close()
			return nil, wire.Shape{}, queryError(ctx, err)
		}
		return cur, wire.Table(fields), nil
	}
	if c.readOnly || p.ReadOnlyAssert {
		return nil, wire.Shape{}, wire.Errorf(wire.KindQuery, "this connection is read-only")
	}
	res, err := c.conn.ExecContext(ctx, text, args...)
	if err != nil {
		return nil, wire.Shape{}, queryError(ctx, err)
	}
	var affected *uint64
	if n, err := res.RowsAffected(); err == nil && n >= 0 {
		u := uint64(n)
		affected = &u
	}
	return nil, wire.Ack(affected), nil
}
