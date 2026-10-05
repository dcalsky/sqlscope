package sqlscope_test

import (
	"fmt"

	"github.com/dcalsky/sqlscope/go"
)

func ExampleApplyRowFilter() {
	sql, err := sqlscope.ApplyRowFilter(
		`SELECT "uid" FROM accounts`,
		`"user" = 'alice'`,
		sqlscope.WithDialect("postgres"),
	)
	if err != nil {
		panic(err)
	}
	fmt.Println(sql)
	// Output: SELECT "uid" FROM (SELECT * FROM accounts WHERE "user" = 'alice') AS accounts
}

func ExampleInjectCTEs() {
	sql, _ := sqlscope.InjectCTEs(
		"SELECT id FROM recent_orders",
		[]sqlscope.CTEDef{{Name: "recent_orders", Query: "SELECT id FROM orders WHERE created_at >= DATE '2026-01-01'"}},
	)
	fmt.Println(sql)
	// Output: WITH recent_orders AS (SELECT id FROM orders WHERE created_at >= CAST('2026-01-01' AS DATE)) SELECT id FROM recent_orders
}

func ExampleColumnOrigins() {
	cols, _ := sqlscope.ColumnOrigins(
		`SELECT o.id, p.amount FROM orders o JOIN payments p ON o.id = p.order_id WHERE o.status = 'PAID'`,
		sqlscope.WithSchema(map[string][]string{"orders": {"id", "status"}, "payments": {"order_id", "amount"}}),
	)
	fmt.Println(cols)
	// Output: map[orders:[id] payments:[amount]]
}

func ExampleOutputColumns() {
	cols, _ := sqlscope.OutputColumns("SELECT id, amount AS total FROM orders")
	fmt.Println(cols)
	// Output: [id total]
}

func ExampleColumnUsages() {
	usages, _ := sqlscope.ColumnUsages("SELECT id FROM orders WHERE status = 'PAID'")
	for _, u := range usages {
		fmt.Println(u.Table, u.Column, u.Clause)
	}
	// Output:
	// orders id SELECT
	// orders status WHERE
}
