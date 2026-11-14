/* Prints the ABI v1 struct layout as JSON. Compared with the golden layout.json. */
#include <stdio.h>
#include "hsearch_module.h"

/* No implicit padding: each struct ends exactly where its last field ends. */
_Static_assert(sizeof(hs_slice) == offsetof(hs_slice, len) + sizeof(uint64_t), "hs_slice padding");
_Static_assert(sizeof(hs_out) == offsetof(hs_out, len) + sizeof(uint64_t), "hs_out padding");
_Static_assert(sizeof(hs_host_v1) == offsetof(hs_host_v1, cancelled) + sizeof(void *), "hs_host_v1 padding");
_Static_assert(sizeof(hs_call) == offsetof(hs_call, call_id) + sizeof(uint64_t), "hs_call padding");
_Static_assert(offsetof(hs_call, reserved0) + sizeof(uint32_t) == offsetof(hs_call, call_id), "hs_call gap");
_Static_assert(offsetof(hs_host_v1, reserved0) + sizeof(uint32_t) == offsetof(hs_host_v1, ctx), "hs_host_v1 gap");

#define F(s, f) printf("    \"%s.%s\": %zu,\n", #s, #f, offsetof(s, f))
#define S(s) printf("    \"%s.size\": %zu,\n", #s, sizeof(s))

int main(void) {
  printf("{\n  \"abi_version\": %u,\n  \"layout\": {\n", HS_ABI_VERSION);
  S(hs_slice); F(hs_slice, ptr); F(hs_slice, len);
  S(hs_out); F(hs_out, ptr); F(hs_out, cap); F(hs_out, len);
  S(hs_host_v1); F(hs_host_v1, struct_size); F(hs_host_v1, reserved0);
  F(hs_host_v1, ctx); F(hs_host_v1, log); F(hs_host_v1, cancelled);
  S(hs_call); F(hs_call, struct_size); F(hs_call, kind); F(hs_call, point);
  F(hs_call, input); F(hs_call, timeout_ms); F(hs_call, reserved0);
  printf("    \"hs_call.call_id\": %zu\n  },\n", offsetof(hs_call, call_id));
  printf("  \"limits\": {\"buffer\": %u, \"descriptor\": %u, \"config\": %u, \"error_detail\": %u},\n",
         HS_MAX_BUFFER_BYTES, HS_MAX_DESCRIPTOR_BYTES, HS_MAX_CONFIG_BYTES, HS_MAX_ERROR_DETAIL_BYTES);
  printf("  \"kinds\": {\"analyzer\": %u, \"query\": %u, \"ingest\": %u, \"scoring\": %u},\n",
         HS_KIND_ANALYZER, HS_KIND_QUERY, HS_KIND_INGEST, HS_KIND_SCORING);
  printf("  \"status\": {\"HS_OK\": %d, \"HS_E_BUFFER_TOO_SMALL\": %d, \"HS_E_INVALID_ARGUMENT\": %d, "
         "\"HS_E_UNSUPPORTED\": %d, \"HS_E_LIMIT\": %d, \"HS_E_CANCELLED\": %d, \"HS_E_MODULE\": %d, "
         "\"HS_E_NOT_INITIALIZED\": %d},\n",
         HS_OK, HS_E_BUFFER_TOO_SMALL, HS_E_INVALID_ARGUMENT, HS_E_UNSUPPORTED, HS_E_LIMIT,
         HS_E_CANCELLED, HS_E_MODULE, HS_E_NOT_INITIALIZED);
  printf("  \"symbols\": [\"%s\", \"%s\", \"%s\", \"%s\", \"%s\"]\n}\n",
         HS_SYM_ABI_VERSION, HS_SYM_INIT, HS_SYM_DESCRIBE, HS_SYM_CALL, HS_SYM_SHUTDOWN);
  return 0;
}
