// Package sqlscope analyzes and rewrites SQL with scope awareness.
//
//	ApplyRowFilter      filter every in-scope table by a predicate
//	InjectCTEs          prepend CTE definitions to a query's root WITH
//	RewriteTables       replace table references with derived tables
//	ColumnOrigins       source columns whose values reach the result (lineage)
//	OutputColumns       names of the columns a statement outputs
//	ReferencedColumns   columns referenced anywhere, per table
//	ColumnUsages        column references with the clause they appear in
//
// Every function takes the same functional [Option]s; options that do not
// apply to a function are ignored. The SQL engine is the Rust sqlscope
// library compiled to WebAssembly and run by wazero, so the package needs
// neither cgo nor native libraries. The engine is compiled on first use; call
// [Init] at startup to pay that cost early. All functions are safe for
// concurrent use.
package sqlscope
