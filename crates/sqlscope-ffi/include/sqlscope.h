/*
 * sqlscope C API: scope-aware SQL analysis and rewriting.
 *
 * Link libsqlscope_ffi (.so / .dylib / .dll, or the static .a / .lib).
 *
 * Every operation goes through sqlscope_call with an operation name and a
 * JSON request; strings are NUL-terminated UTF-8. The JSON response is
 * either {"ok": <result>} or {"error": {"kind": "...", "message": "..."}}
 * with kind one of "invalid_argument", "parse", "unsupported", "internal".
 *
 * Operations: apply_row_filter, inject_ctes, rewrite_tables, column_origins,
 * output_columns, referenced_columns, column_usages. The request is
 *
 *   {"sql": "...", "predicate": "...", "ctes": [{"name", "query"}],
 *    "rewrites": [{"matchKey", "inline": TableRef, "union": {"tableAlias",
 *    "columns", "branches": [TableRef]}}],
 *    "options": {"dialect", "schema": {table: [column]}, "tableNames",
 *    "tablePatterns", "defaultDb", "stripCatalogs"}}
 *
 * where TableRef is {"table", "schema", "catalog"}; only "sql" is required.
 * All functions are safe to call concurrently from any thread.
 */
#ifndef SQLSCOPE_H
#define SQLSCOPE_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* The ABI version this header describes. */
#define SQLSCOPE_ABI_VERSION 1

/* The ABI version implemented by the library; compare with SQLSCOPE_ABI_VERSION. */
uint32_t sqlscope_abi_version(void);

/* The library version ("X.Y.Z"), a static string that must not be freed. */
const char *sqlscope_version(void);

/*
 * Runs `operation` on the JSON `request` and returns the JSON response.
 * Never returns NULL. Release the response with sqlscope_free.
 */
char *sqlscope_call(const char *operation, const char *request);

/* Releases a response returned by sqlscope_call. NULL is ignored. */
void sqlscope_free(char *response);

#ifdef __cplusplus
}
#endif

#endif /* SQLSCOPE_H */
