package sqlscope_test

import (
	"errors"
	"fmt"
	"reflect"
	"strings"
	"sync"
	"testing"

	"github.com/dcalsky/sqlscope/go"
)

func TestMain(m *testing.M) {
	if err := sqlscope.Load(""); err != nil {
		panic(err)
	}
	m.Run()
}

func TestLibrary(t *testing.T) {
	version, err := sqlscope.LibraryVersion()
	if err != nil || version == "" {
		t.Fatalf("LibraryVersion: %q %v", version, err)
	}
	if err := sqlscope.Load(""); err != nil {
		t.Fatalf("reloading the default library: %v", err)
	}
	if err := sqlscope.Load("/nonexistent/" + sqlscope.LibraryFileName()); err == nil {
		t.Fatal("want an error loading a second library")
	}
}

func TestApplyRowFilter(t *testing.T) {
	cases := []struct {
		name, sql, want string
		opts            []sqlscope.Option
	}{
		{
			name: "postgres",
			sql:  "SELECT id FROM orders",
			want: "SELECT id FROM (SELECT * FROM orders WHERE tenant_id = 7) AS orders",
			opts: []sqlscope.Option{sqlscope.WithDialect("postgres")},
		},
		{
			name: "scoped to each query",
			sql:  "SELECT t.a FROM s1.t WHERE t.b IN (SELECT t.c FROM s2.t)",
			want: "SELECT s1_t.a FROM (SELECT * FROM s1.t WHERE tenant_id = 7) AS s1_t WHERE s1_t.b IN (SELECT s2_t.c FROM (SELECT * FROM s2.t WHERE tenant_id = 7) AS s2_t)",
		},
		{
			name: "table names",
			sql:  "SELECT * FROM a JOIN c ON a.id = c.id",
			want: "SELECT * FROM (SELECT * FROM a WHERE tenant_id = 7) AS a JOIN c ON a.id = c.id",
			opts: []sqlscope.Option{sqlscope.WithTableNames("a")},
		},
		{
			name: "default db",
			sql:  "SELECT * FROM orders",
			want: "SELECT * FROM (SELECT * FROM orders WHERE tenant_id = 7) AS orders",
			opts: []sqlscope.Option{sqlscope.WithTableNames("sales.orders"), sqlscope.WithDefaultDB("sales")},
		},
		{
			name: "regexp out of scope is a no-op",
			sql:  "select * from  logs -- untouched",
			want: "select * from  logs -- untouched",
			opts: []sqlscope.Option{sqlscope.WithTableRegexp(`^audit_`)},
		},
		{
			name: "cte reference is not wrapped",
			sql:  "WITH c AS (SELECT * FROM a) SELECT * FROM c",
			want: "WITH c AS (SELECT * FROM (SELECT * FROM a WHERE tenant_id = 7) AS a) SELECT * FROM c",
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got, err := sqlscope.ApplyRowFilter(tc.sql, "tenant_id = 7", tc.opts...)
			if err != nil {
				t.Fatal(err)
			}
			if got != tc.want {
				t.Fatalf("\n got: %s\nwant: %s", got, tc.want)
			}
		})
	}
}

func TestInjectCTEs(t *testing.T) {
	got, err := sqlscope.InjectCTEs(
		"WITH large AS (SELECT id FROM foo WHERE amount > 100) SELECT * FROM large",
		[]sqlscope.CTEDef{{Name: "foo", Query: "SELECT id, amount FROM orders -- note"}},
	)
	if err != nil {
		t.Fatal(err)
	}
	const want = "WITH foo AS (SELECT id, amount FROM orders), large AS (SELECT id FROM foo WHERE amount > 100) SELECT * FROM large"
	if got != want {
		t.Fatalf("\n got: %s\nwant: %s", got, want)
	}
	if got, err := sqlscope.InjectCTEs("not sql", nil); err != nil || got != "not sql" {
		t.Fatalf("empty ctes must be a no-op: %q %v", got, err)
	}
	_, err = sqlscope.InjectCTEs("WITH foo AS (SELECT 1) SELECT * FROM foo", []sqlscope.CTEDef{{Name: "FOO", Query: "SELECT 2"}})
	if !errors.Is(err, sqlscope.ErrInvalidArgument) {
		t.Fatalf("want ErrInvalidArgument, got %v", err)
	}
}

func TestRewriteTables(t *testing.T) {
	inline := sqlscope.TableRewrite{
		MatchKey: "myschema.mytable",
		Inline:   &sqlscope.TableRef{Catalog: "cat", Schema: "vsch", Table: "view1"},
	}
	union := sqlscope.TableRewrite{
		MatchKey: "myschema.mytable",
		Union: &sqlscope.UnionRewrite{
			TableAlias: "replacement",
			Columns:    []string{"col1"},
			Branches:   []sqlscope.TableRef{{Schema: "s", Table: "v1"}, {Schema: "s", Table: "v2"}},
		},
	}
	cases := []struct {
		sql, want string
		rewrite   sqlscope.TableRewrite
		opts      []sqlscope.Option
	}{
		{
			sql:     "SELECT vdm.myschema.mytable.col1 FROM vdm.myschema.mytable",
			want:    "SELECT mytable.col1 FROM (SELECT * FROM cat.vsch.view1) AS mytable",
			rewrite: inline,
			opts:    []sqlscope.Option{sqlscope.WithStripCatalogs("vdm")},
		},
		{
			sql:     "SELECT mytable.col1 FROM myschema.mytable",
			want:    "SELECT replacement.col1 FROM (SELECT col1 FROM s.v1 UNION DISTINCT SELECT col1 FROM s.v2) AS replacement",
			rewrite: union,
		},
		{
			sql:     "SELECT * FROM other.myschema.mytable",
			want:    "SELECT * FROM other.myschema.mytable",
			rewrite: inline,
		},
	}
	for _, tc := range cases {
		got, err := sqlscope.RewriteTables(tc.sql, []sqlscope.TableRewrite{tc.rewrite}, tc.opts...)
		if err != nil {
			t.Fatal(err)
		}
		if got != tc.want {
			t.Fatalf("\n got: %s\nwant: %s", got, tc.want)
		}
	}
	_, err := sqlscope.RewriteTables("SELECT 1", []sqlscope.TableRewrite{{MatchKey: "s.t"}})
	if !errors.Is(err, sqlscope.ErrInvalidArgument) {
		t.Fatalf("want ErrInvalidArgument, got %v", err)
	}
}

var lineageSchema = map[string][]string{
	"hive.raw.users":  {"user_id", "user_name", "email"},
	"hive.raw.orders": {"order_id", "user_id", "amount", "status"},
}

func TestColumnOrigins(t *testing.T) {
	cases := []struct {
		sql  string
		want map[string][]string
	}{
		{
			"CREATE VIEW v AS SELECT u.user_name, o.* FROM hive.raw.users u JOIN hive.raw.orders o ON u.user_id = o.user_id WHERE o.status = 'PAID'",
			map[string][]string{"hive.raw.orders": {"amount", "order_id", "status", "user_id"}, "hive.raw.users": {"user_name"}},
		},
		{
			"SELECT user_id FROM hive.raw.users EXCEPT SELECT user_id FROM hive.raw.orders",
			map[string][]string{"hive.raw.orders": {}, "hive.raw.users": {"user_id"}},
		},
		{
			"SELECT count(*) AS c FROM hive.raw.orders",
			map[string][]string{"hive.raw.orders": {}},
		},
		{
			"UPDATE hive.raw.orders SET amount = (SELECT max(u.user_id) FROM hive.raw.users u)",
			map[string][]string{"hive.raw.orders": {}, "hive.raw.users": {"user_id"}},
		},
		{"DROP TABLE t", map[string][]string{}},
	}
	for _, tc := range cases {
		got, err := sqlscope.ColumnOrigins(tc.sql, sqlscope.WithSchema(lineageSchema))
		if err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(got, tc.want) {
			t.Fatalf("%s\n got: %v\nwant: %v", tc.sql, got, tc.want)
		}
	}
}

func TestOutputColumns(t *testing.T) {
	got, err := sqlscope.OutputColumns("SELECT id, amount AS total, count(*) FROM orders")
	if err != nil || !reflect.DeepEqual(got, []string{"id", "total", "_col2"}) {
		t.Fatalf("got %v, %v", got, err)
	}
	got, err = sqlscope.OutputColumns("SELECT * FROM hive.raw.users", sqlscope.WithSchema(lineageSchema))
	if err != nil || !reflect.DeepEqual(got, []string{"user_id", "user_name", "email"}) {
		t.Fatalf("got %v, %v", got, err)
	}
	got, err = sqlscope.OutputColumns("DROP TABLE t")
	if err != nil || got != nil {
		t.Fatalf("want nil, got %v, %v", got, err)
	}
	if _, err := sqlscope.OutputColumns("INSERT INTO t VALUES (1)"); !errors.Is(err, sqlscope.ErrUnsupported) {
		t.Fatalf("want ErrUnsupported, got %v", err)
	}
}

func TestReferencedColumnsAndUsages(t *testing.T) {
	const sql = "SELECT a, b FROM t GROUP BY 1, b ORDER BY a"
	got, err := sqlscope.ReferencedColumns(sql)
	if err != nil || !reflect.DeepEqual(got, map[string][]string{"t": {"a", "b"}}) {
		t.Fatalf("got %v, %v", got, err)
	}
	usages, err := sqlscope.ColumnUsages(sql)
	if err != nil {
		t.Fatal(err)
	}
	want := []sqlscope.ColumnUsage{
		{Table: "t", Column: "a", Clause: sqlscope.ColumnClauseGroupBy},
		{Table: "t", Column: "a", Clause: sqlscope.ColumnClauseOrderBy},
		{Table: "t", Column: "a", Clause: sqlscope.ColumnClauseSelect},
		{Table: "t", Column: "b", Clause: sqlscope.ColumnClauseGroupBy},
		{Table: "t", Column: "b", Clause: sqlscope.ColumnClauseSelect},
	}
	if !reflect.DeepEqual(usages, want) {
		t.Fatalf("\n got: %v\nwant: %v", usages, want)
	}
	usages, err = sqlscope.ColumnUsages("SELECT 1")
	if err != nil || usages == nil || len(usages) != 0 {
		t.Fatalf("want empty non-nil, got %#v, %v", usages, err)
	}
	got, err = sqlscope.ReferencedColumns(
		"UPDATE t SET t.x = t.y + 1 WHERE t.a > 1", sqlscope.WithDialect("mysql"))
	if err != nil || !reflect.DeepEqual(got, map[string][]string{"t": {"a", "x", "y"}}) {
		t.Fatalf("got %v, %v", got, err)
	}
}

func TestErrors(t *testing.T) {
	cases := []struct {
		name string
		call func() error
		want error
	}{
		{"parse", func() error { _, err := sqlscope.ReferencedColumns("SELECT FROM WHERE"); return err }, sqlscope.ErrParse},
		{"unsupported", func() error { _, err := sqlscope.ApplyRowFilter("DELETE FROM t", "x = 1"); return err }, sqlscope.ErrUnsupported},
		{"multiple statements", func() error { _, err := sqlscope.ColumnOrigins("SELECT 1; SELECT 2"); return err }, sqlscope.ErrUnsupported},
		{"empty predicate", func() error { _, err := sqlscope.ApplyRowFilter("SELECT 1", " "); return err }, sqlscope.ErrInvalidArgument},
		{"unknown dialect", func() error { _, err := sqlscope.OutputColumns("SELECT 1", sqlscope.WithDialect("nope")); return err }, sqlscope.ErrInvalidArgument},
		{"empty table names", func() error {
			_, err := sqlscope.ApplyRowFilter("SELECT * FROM t", "x = 1", sqlscope.WithTableNames())
			return err
		}, sqlscope.ErrInvalidArgument},
		{"bad regexp", func() error {
			_, err := sqlscope.ApplyRowFilter("SELECT * FROM t", "x = 1", sqlscope.WithTableRegexp("("))
			return err
		}, sqlscope.ErrInvalidArgument},
		{"nil option", func() error { _, err := sqlscope.ReferencedColumns("SELECT 1", nil); return err }, sqlscope.ErrInvalidArgument},
		{"injection", func() error { _, err := sqlscope.ApplyRowFilter("SELECT * FROM t", "1=1) AS x --"); return err }, sqlscope.ErrParse},
		{"oversized", func() error {
			_, err := sqlscope.ReferencedColumns("SELECT 1 FROM t WHERE x IN (" + strings.Repeat("1,", 1<<20) + "1)")
			return err
		}, sqlscope.ErrUnsupported},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			err := tc.call()
			if !errors.Is(err, tc.want) {
				t.Fatalf("want %v, got %v", tc.want, err)
			}
			var typed *sqlscope.Error
			if !errors.As(err, &typed) || typed.Message == "" {
				t.Fatalf("want *sqlscope.Error with a message, got %#v", err)
			}
		})
	}
}

// The library accepts deep nesting on goroutine-driven threads and rejects
// deeper input cleanly instead of overflowing the stack.
func TestDepthLimits(t *testing.T) {
	nested := func(depth int) string {
		sql := "SELECT a FROM t"
		for i := 0; i < depth; i++ {
			sql = fmt.Sprintf("SELECT a FROM (%s) x%d", sql, i)
		}
		return sql
	}
	parens := func(depth int) string {
		return "SELECT " + strings.Repeat("(", depth) + "a" + strings.Repeat(")", depth) + " AS a FROM t"
	}
	rewrite := []sqlscope.TableRewrite{{MatchKey: "s.t", Inline: &sqlscope.TableRef{Table: "u"}}}
	for _, sql := range []string{nested(120), parens(200)} {
		if _, err := sqlscope.ApplyRowFilter(sql, "x = 1"); err != nil {
			t.Fatalf("ApplyRowFilter: %v", err)
		}
		if _, err := sqlscope.InjectCTEs(sql, []sqlscope.CTEDef{{Name: "c", Query: "SELECT 1"}}); err != nil {
			t.Fatalf("InjectCTEs: %v", err)
		}
		if _, err := sqlscope.RewriteTables(sql, rewrite); err != nil {
			t.Fatalf("RewriteTables: %v", err)
		}
		if got, err := sqlscope.ColumnOrigins(sql); err != nil || !reflect.DeepEqual(got["t"], []string{"a"}) {
			t.Fatalf("ColumnOrigins: %v %v", got, err)
		}
		if _, err := sqlscope.OutputColumns(sql); err != nil {
			t.Fatalf("OutputColumns: %v", err)
		}
		if _, err := sqlscope.ColumnUsages(sql); err != nil {
			t.Fatalf("ColumnUsages: %v", err)
		}
	}
	for _, sql := range []string{nested(400), parens(4000)} {
		if _, err := sqlscope.ApplyRowFilter(sql, "x = 1"); !errors.Is(err, sqlscope.ErrUnsupported) {
			t.Fatalf("want ErrUnsupported, got %v", err)
		}
	}
	// The library stays usable after rejecting input.
	if _, err := sqlscope.ReferencedColumns("SELECT a FROM t"); err != nil {
		t.Fatal(err)
	}
}

func TestConcurrentUse(t *testing.T) {
	var wg sync.WaitGroup
	errs := make(chan error, 64)
	for i := 0; i < 64; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			col := fmt.Sprintf("c%d", i)
			got, err := sqlscope.ReferencedColumns(fmt.Sprintf("SELECT %s FROM t%d", col, i))
			if err != nil {
				errs <- err
				return
			}
			if want := map[string][]string{fmt.Sprintf("t%d", i): {col}}; !reflect.DeepEqual(got, want) {
				errs <- fmt.Errorf("got %v, want %v", got, want)
			}
		}(i)
	}
	wg.Wait()
	close(errs)
	for err := range errs {
		t.Error(err)
	}
}

func BenchmarkColumnOrigins(b *testing.B) {
	const sql = "SELECT o.order_id, u.user_name FROM hive.raw.orders o JOIN hive.raw.users u ON o.user_id = u.user_id WHERE o.status = 'PAID'"
	opt := sqlscope.WithSchema(lineageSchema)
	b.RunParallel(func(pb *testing.PB) {
		for pb.Next() {
			if _, err := sqlscope.ColumnOrigins(sql, opt); err != nil {
				b.Fatal(err)
			}
		}
	})
}
