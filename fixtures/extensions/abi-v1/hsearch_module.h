/*
 * hsearch extension module ABI, version 1.0 (golden fixture, M24.01).
 *
 * The contract is docs/plan/extension-modules.md. This header is a fixture: a
 * change to it changes the ABI and must come with a version bump, a
 * regenerated layout.json and a note in that document. Supported targets are
 * LP64 (x86_64 and aarch64 Linux and macOS); 32-bit native targets are not.
 *
 * Ownership, in one rule: the host owns every buffer that crosses the
 * boundary and lends it for one call. A module never returns memory the host
 * must free, and never keeps a pointer it was lent after the call returns.
 */
#ifndef HSEARCH_MODULE_H
#define HSEARCH_MODULE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32)
#define HS_EXPORT __declspec(dllexport)
#else
#define HS_EXPORT __attribute__((visibility("default")))
#endif

/* Packed as (major << 16) | minor. A module's major must equal the host's and
 * its minor must not exceed the host's; minors only add fields at the end of
 * a struct_size-prefixed struct or add values. */
#define HS_ABI_VERSION_MAJOR 1u
#define HS_ABI_VERSION_MINOR 0u
#define HS_ABI_VERSION ((HS_ABI_VERSION_MAJOR << 16) | HS_ABI_VERSION_MINOR)

/* Hard ABI ceilings. A node's configured limits are always at or below them. */
#define HS_MAX_BUFFER_BYTES (16u * 1024u * 1024u)
#define HS_MAX_DESCRIPTOR_BYTES (16u * 1024u)
#define HS_MAX_CONFIG_BYTES (64u * 1024u)
#define HS_MAX_ERROR_DETAIL_BYTES 256u

/* Extension point kinds (hs_call.kind). */
#define HS_KIND_ANALYZER 1u
#define HS_KIND_QUERY 2u
#define HS_KIND_INGEST 3u
#define HS_KIND_SCORING 4u

/* Log levels (hs_host_v1.log). */
#define HS_LOG_ERROR 1u
#define HS_LOG_WARN 2u
#define HS_LOG_INFO 3u
#define HS_LOG_DEBUG 4u

/* Status codes a module returns. Zero is success; every failure is negative.
 * Traps, fuel, memory, timeout and malformed output are host-side outcomes
 * and are not module return codes. */
#define HS_OK 0
#define HS_E_BUFFER_TOO_SMALL (-1) /* out->len is set to the size required */
#define HS_E_INVALID_ARGUMENT (-2)
#define HS_E_UNSUPPORTED (-3) /* unknown kind, point or version */
#define HS_E_LIMIT (-4)       /* the module refused an over-limit input */
#define HS_E_CANCELLED (-5)
#define HS_E_MODULE (-6) /* module failure; optional UTF-8 detail in out */
#define HS_E_NOT_INITIALIZED (-7)

/* A borrowed, read-only byte range. Valid only for the duration of the call. */
typedef struct hs_slice {
  const uint8_t *ptr;
  uint64_t len;
} hs_slice;

/* A host-provided output buffer. The module writes at most cap bytes and sets
 * len to the number written, or, on HS_E_BUFFER_TOO_SMALL, the number needed.
 * The host retries at most once and only if the need is within its limit. */
typedef struct hs_out {
  uint8_t *ptr;
  uint64_t cap;
  uint64_t len;
} hs_out;

/* The only services a module is granted. There is no file, socket, clock,
 * randomness or environment access: a pure module needs none, and a call must
 * be reproducible from its input. Callbacks may be called from any thread
 * that is inside hs_module_call and only during that call. */
typedef struct hs_host_v1 {
  uint32_t struct_size; /* sizeof of the host's version of this struct */
  uint32_t reserved0;   /* zero */
  void *ctx;            /* passed back as the first argument of each callback */
  void (*log)(void *ctx, uint32_t level, const uint8_t *msg, uint64_t len);
  int32_t (*cancelled)(void *ctx); /* nonzero when the caller gave up */
} hs_host_v1;

/* One call into an extension point. */
typedef struct hs_call {
  uint32_t struct_size; /* sizeof of the host's version of this struct */
  uint32_t kind;        /* HS_KIND_* */
  hs_slice point;       /* the extension point id, UTF-8, as declared */
  hs_slice input;       /* the point's input encoding */
  uint32_t timeout_ms;  /* advisory for native modules, enforced for WASM */
  uint32_t reserved0;   /* zero */
} hs_call;

/* Exports. Names are the contract (HS_SYM_*); a module exports exactly these. */
#define HS_SYM_ABI_VERSION "hs_abi_version"
#define HS_SYM_INIT "hs_module_init"
#define HS_SYM_DESCRIBE "hs_module_describe"
#define HS_SYM_CALL "hs_module_call"
#define HS_SYM_SHUTDOWN "hs_module_shutdown"

/* Called first, alone, before any other symbol is resolved or called. */
HS_EXPORT uint32_t hs_abi_version(void);
/* Called once. config is the node configuration's opaque module section. */
HS_EXPORT int32_t hs_module_init(const hs_host_v1 *host, hs_slice config);
/* Writes the UTF-8 JSON descriptor (see the specification). Pure. */
HS_EXPORT int32_t hs_module_describe(hs_out *out);
/* May be called concurrently up to the descriptor's max_concurrent_calls. */
HS_EXPORT int32_t hs_module_call(const hs_call *call, hs_out *out);
/* Called once, after the last call has returned. */
HS_EXPORT void hs_module_shutdown(void);

typedef uint32_t (*hs_abi_version_fn)(void);
typedef int32_t (*hs_module_init_fn)(const hs_host_v1 *host, hs_slice config);
typedef int32_t (*hs_module_describe_fn)(hs_out *out);
typedef int32_t (*hs_module_call_fn)(const hs_call *call, hs_out *out);
typedef void (*hs_module_shutdown_fn)(void);

#ifdef __cplusplus
}
#endif

#endif /* HSEARCH_MODULE_H */
