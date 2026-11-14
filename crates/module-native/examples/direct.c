/* Floor for the pooled call: the same module called straight from one thread, no worker
 * hop, no copy in or out. Built and run by examples/bench.rs; not part of the product. */
#define HS_MODULE_BUILD 1
#include "hsearch_module.h"
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static double now_ns(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (double)ts.tv_sec * 1e9 + (double)ts.tv_nsec;
}

int main(int argc, char **argv) {
  if (argc < 4) return 2;
  void *lib = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
  if (!lib) return 3;
  hs_module_init_fn init = (hs_module_init_fn)dlsym(lib, "hs_module_init");
  hs_module_call_fn call = (hs_module_call_fn)dlsym(lib, "hs_module_call");
  if (!init || !call) return 4;
  hs_host_v1 host = { sizeof host, 0, NULL, NULL, NULL };
  hs_slice cfg = { (const uint8_t *)"", 0 };
  if (init(&host, cfg) != HS_OK) return 5;
  size_t payload = (size_t)atol(argv[2]);
  long iters = atol(argv[3]);
  uint8_t *in = malloc(payload + 1), *buf = malloc(payload + 16);
  in[0] = 'e';
  memset(in + 1, 'a', payload);
  hs_call c = { sizeof c, HS_KIND_ANALYZER, { (const uint8_t *)"echo", 4 }, { in, payload + 1 }, 1000, 0, 1 };
  double *t = malloc(sizeof(double) * (size_t)iters);
  for (long i = 0; i < iters; i++) {
    hs_out out = { buf, payload + 16, 0 };
    double a = now_ns();
    int32_t rc = call(&c, &out);
    t[i] = now_ns() - a;
    if (rc != HS_OK || out.len != payload) return 6;
  }
  int cmp(const void *x, const void *y) { double a = *(const double *)x, b = *(const double *)y; return (a > b) - (a < b); }
  qsort(t, (size_t)iters, sizeof(double), cmp);
  printf("%.0f %.0f\n", t[iters / 2], t[(long)((double)iters * 0.99)]);
  return 0;
}
