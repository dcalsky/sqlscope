/* Links the FFI library from C and runs one operation. */
#include <stdio.h>
#include <string.h>

#include "sqlscope.h"

int main(void) {
    if (sqlscope_abi_version() != SQLSCOPE_ABI_VERSION) {
        fprintf(stderr, "ABI mismatch: %u\n", sqlscope_abi_version());
        return 1;
    }
    char *response = sqlscope_call(
        "apply_row_filter",
        "{\"sql\": \"SELECT id FROM orders\", \"predicate\": \"tenant_id = 7\"}");
    const char *want = "{\"ok\":\"SELECT id FROM (SELECT * FROM orders WHERE tenant_id = 7) AS orders\"}";
    int ok = strcmp(response, want) == 0;
    printf("sqlscope %s: %s\n", sqlscope_version(), response);
    sqlscope_free(response);
    return ok ? 0 : 1;
}
