//! The unit tests of sql-guard, the Go library sqlscope grew out of, ported
//! case for case. Each test keeps the name of the Go test it comes from
//! (`TestBughuntLateral` -> `bughunt_lateral`) so the two suites can be
//! compared. Go-only machinery (FFI bundling, the polyglot client contract,
//! benchmarks, functional-option plumbing) has no counterpart and is not
//! ported; where sqlscope deliberately differs, the test says so.
#![allow(clippy::type_complexity)]

mod common;

mod sqlguard {
    pub mod bind_ctes;
    pub mod catalog_qualified;
    pub mod invariants;
    pub mod lineage;
    pub mod parse_columns;
    pub mod referenced_columns;
    pub mod rewrite_tables;
    pub mod row_filter;
}
