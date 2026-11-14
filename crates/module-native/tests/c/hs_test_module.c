/* A deliberately misbehaving module for the loader tests. Built by tests/load.rs with
 * cc; the compile-time variants (-D...) break one contract rule each, and the first
 * input byte of a call picks a runtime behaviour, so one library covers many paths. */
#define HS_MODULE_BUILD 1
#include "hsearch_module.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#ifndef ABI_RET
#define ABI_RET HS_ABI_VERSION
#endif

static const hs_host_v1 *g_host __attribute__((unused));
static char g_marker[512] __attribute__((unused));

HS_EXPORT uint32_t hs_abi_version(void) { return ABI_RET; }

#ifndef ONLY_VERSION
HS_EXPORT int32_t hs_module_init(const hs_host_v1 *host, hs_slice config) {
#ifdef INIT_FAIL
  (void)host; (void)config;
  return HS_E_MODULE;
#else
  g_host = host;
  if (config.len > 0 && config.len < sizeof g_marker) {
    memcpy(g_marker, config.ptr, config.len);
    g_marker[config.len] = 0;
  }
  return HS_OK;
#endif
}

HS_EXPORT int32_t hs_module_describe(hs_out *out) {
  const char *json = DESC_JSON;
  size_t n = strlen(json);
#ifdef DESC_LIE
  memcpy(out->ptr, json, n < out->cap ? n : out->cap);
  out->len = out->cap + 1;
  return HS_OK;
#elif defined(DESC_FAIL)
  (void)json; (void)n; (void)out;
  return HS_E_MODULE;
#elif defined(DESC_OVERRUN)
  (void)json; (void)n;
  memset(out->ptr, ' ', out->cap + 8);
  out->len = 2;
  return HS_OK;
#else
  if (n > out->cap) { out->len = n; return HS_E_BUFFER_TOO_SMALL; }
  memcpy(out->ptr, json, n);
  out->len = n;
  return HS_OK;
#endif
}

__attribute__((unused)) static void nap_ms(long ms) {
  struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
  nanosleep(&ts, NULL);
}

__attribute__((unused)) static uint32_t le32(const uint8_t *p) {
  return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) | ((uint32_t)p[3] << 24);
}

#ifndef OMIT_CALL
HS_EXPORT int32_t hs_module_call(const hs_call *call, hs_out *out) {
  const uint8_t *in = call->input.ptr;
  uint64_t n = call->input.len;
  if (n == 0) return HS_E_INVALID_ARGUMENT;
  switch (in[0]) {
  case 'e': { /* echo the rest */
    uint64_t m = n - 1;
    if (m > out->cap) { out->len = m; return HS_E_BUFFER_TOO_SMALL; }
    memcpy(out->ptr, in + 1, m);
    out->len = m;
    return HS_OK;
  }
  case 'n': { /* need N bytes: too small until the buffer holds N, then N 'x' */
    uint64_t need = le32(in + 1);
    if (need > out->cap) { out->len = need; return HS_E_BUFFER_TOO_SMALL; }
    memset(out->ptr, 'x', need);
    out->len = need;
    return HS_OK;
  }
  case 'l': out->len = out->cap + 1; return HS_OK;                       /* length lie */
  case 'k': out->len = 1; return HS_E_BUFFER_TOO_SMALL;                  /* "need" no larger than cap */
  case 'w': out->len = out->cap + 1; return HS_E_BUFFER_TOO_SMALL;      /* grows forever */
  case 'u': return 5;                                                    /* undefined status */
  case 'g': { /* a status from the next byte, with detail for HS_E_MODULE */
    int8_t code = (int8_t)in[1];
    if (code == HS_E_MODULE) { memcpy(out->ptr, "boom /etc/shadow", 16); out->len = 16; }
    return code;
  }
  case 'o': memset(out->ptr, 'z', out->cap + 8); out->len = 1; return HS_OK; /* overrun */
  case 's': { /* cooperative sleep: ms in next 4 bytes, stops when cancelled */
    long left = (long)le32(in + 1);
    while (left > 0) {
      if (g_host->cancelled(g_host->ctx, call->call_id)) return HS_E_CANCELLED;
      nap_ms(5);
      left -= 5;
    }
    out->len = 0;
    return HS_OK;
  }
  case 'h': { /* hang: sleep ignoring cancellation */
    nap_ms((long)le32(in + 1));
    out->len = 0;
    return HS_OK;
  }
  case 'L': { /* log a few lines, one over 1 KiB */
    static char big[3000];
    memset(big, 'a', sizeof big);
    g_host->log(g_host->ctx, call->call_id, HS_LOG_INFO, (const uint8_t *)"hello", 5);
    g_host->log(g_host->ctx, call->call_id, HS_LOG_WARN, (const uint8_t *)big, sizeof big);
    out->len = 0;
    return HS_OK;
  }
  case 'i': { /* report the call id, the kind and the point id back */
    uint64_t id = call->call_id;
    memcpy(out->ptr, &id, 8);
    memcpy(out->ptr + 8, &call->kind, 4);
    memcpy(out->ptr + 12, call->point.ptr, call->point.len);
    out->len = 12 + call->point.len;
    return HS_OK;
  }
  default: return HS_E_UNSUPPORTED;
  }
}
#endif

HS_EXPORT void hs_module_shutdown(void) {
  if (g_marker[0]) {
    FILE *f = fopen(g_marker, "w");
    if (f) { fputs("shutdown\n", f); fclose(f); }
  }
}
#endif
