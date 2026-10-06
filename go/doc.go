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
// apply to a function are ignored.
//
// The SQL engine is the sqlscope FFI shared library (libsqlscope_ffi.so,
// libsqlscope_ffi.dylib or sqlscope_ffi.dll) published with each sqlscope
// release. The package loads it at runtime without cgo, so CGO_ENABLED=0
// builds work; it neither bundles nor downloads the library. See [Load] for
// how the library is located. All functions are safe for concurrent use.
package sqlscope
