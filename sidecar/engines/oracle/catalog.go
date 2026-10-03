package main

import (
	"context"
	"strings"

	"github.com/chud-lori/datagrep/sidecar/common/wire"
)

func (c *oraConn) Children(ctx context.Context, p wire.ChildrenParams) ([]wire.Node, error) {
	prefix := ""
	if p.Prefix != nil {
		prefix = *p.Prefix
	}
	limit := max(p.Limit, 1)
	var query string
	args := []any{prefix}
	switch len(p.Path) {
	case 0:
		query = `SELECT username, 'Schema' FROM all_users
			WHERE username LIKE :1 || '%' ORDER BY username FETCH FIRST :2 ROWS ONLY`
	case 1:
		query = `SELECT object_name, INITCAP(object_type) FROM all_objects
			WHERE owner = :1 AND object_type IN ('TABLE', 'VIEW') AND object_name LIKE :2 || '%'
			ORDER BY object_name FETCH FIRST :3 ROWS ONLY`
		args = []any{p.Path[0], prefix}
	case 2:
		query = `SELECT column_name, 'Column' FROM all_tab_columns
			WHERE owner = :1 AND table_name = :2 AND column_name LIKE :3 || '%'
			ORDER BY column_id FETCH FIRST :4 ROWS ONLY`
		args = []any{p.Path[0], p.Path[1], prefix}
	default:
		return nil, nil
	}
	rows, err := c.conn.QueryContext(ctx, query, append(args, int64(limit))...)
	if err != nil {
		return nil, queryError(ctx, err)
	}
	defer rows.Close()
	var nodes []wire.Node
	for rows.Next() {
		var name, kind string
		if err := rows.Scan(&name, &kind); err != nil {
			return nil, queryError(ctx, err)
		}
		path := append(append([]string{}, p.Path...), name)
		nodes = append(nodes, wire.Node{Path: path, Kind: kind, HasChildren: kind != wire.KindColumn})
	}
	return nodes, queryError(ctx, rows.Err())
}

func (c *oraConn) Describe(ctx context.Context, path []string) (wire.DescribeReply, error) {
	if len(path) != 2 {
		return wire.DescribeReply{}, wire.Errorf(wire.KindUnsupported, "describe needs schema.table")
	}
	rows, err := c.conn.QueryContext(ctx, `SELECT column_name, data_type, NVL(data_precision, 0),
		NVL(data_scale, -1), nullable FROM all_tab_columns
		WHERE owner = :1 AND table_name = :2 ORDER BY column_id`, path[0], path[1])
	if err != nil {
		return wire.DescribeReply{}, queryError(ctx, err)
	}
	defer rows.Close()
	reply := wire.DescribeReply{Node: wire.Node{Path: path, Kind: wire.KindTable, HasChildren: true}}
	for rows.Next() {
		var name, dataType, nullable string
		var precision, scale int64
		if err := rows.Scan(&name, &dataType, &precision, &scale, &nullable); err != nil {
			return wire.DescribeReply{}, queryError(ctx, err)
		}
		native := dataType
		reply.Fields = append(reply.Fields, wire.Field{
			Name:       name,
			Logical:    logicalFor(dictionaryType(dataType), precision, scale, scale >= 0),
			Nullable:   nullable == "Y",
			NativeType: &native,
		})
	}
	if len(reply.Fields) == 0 {
		return wire.DescribeReply{}, wire.Errorf(wire.KindQuery, "no table %s.%s", path[0], path[1])
	}
	return reply, queryError(ctx, rows.Err())
}

// dictionaryType folds the data dictionary's spelling onto the names go-ora reports.
func dictionaryType(t string) string {
	switch {
	case t == "BINARY_FLOAT" || t == "BINARY_DOUBLE":
		return "IBDouble"
	case t == "BLOB" || t == "LONG RAW":
		return "RAW"
	case strings.HasSuffix(t, "WITH LOCAL TIME ZONE"):
		return "TimeStampeLTZ"
	case strings.HasSuffix(t, "WITH TIME ZONE"):
		return "TIMESTAMPTZ"
	case strings.HasPrefix(t, "TIMESTAMP"):
		return "TIMESTAMP"
	}
	return t
}
