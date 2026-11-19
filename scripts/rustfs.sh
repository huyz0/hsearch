#!/usr/bin/env bash
# Start the digest-pinned RustFS from dev/images.json on localhost:9000 and wait until it
# answers. Prints `export` lines for the S3 test environment. Local use and CI:
#   eval "$(scripts/rustfs.sh up)"   ...   scripts/rustfs.sh restart   ...   scripts/rustfs.sh down
# `health` checks a running instance with a signed write and read; `logs` prints its container log.
set -euo pipefail
cd "$(dirname "$0")/.."
NAME="${RUSTFS_NAME:-hsearch-rustfs}"
IMAGE="$(python3 -c 'import json;print(json.load(open("dev/images.json"))["rustfs"])')"
ACCESS=hsearchtest
SECRET=hsearchtestsecret

# One signed round trip: make a scratch bucket, write an object, read it back, delete it. Any
# HTTP answer, even 403, only proves the listener is bound; RustFS can answer 503 to a signed
# request for a while after that, so readiness is a successful signed write and read.
S3="http://127.0.0.1:9000"
signed() { curl -s --max-time 5 --aws-sigv4 "aws:amz:us-east-1:s3" --user "$ACCESS:$SECRET" "$@"; }
healthy() {
  local bucket="hsearch-health" body="ok-$$-$RANDOM" code got
  code="$(signed -o /dev/null -w '%{http_code}' -X PUT "$S3/$bucket" || true)"
  case "$code" in 200|409) ;; *) return 1 ;; esac
  code="$(signed -o /dev/null -w '%{http_code}' -X PUT --data "$body" "$S3/$bucket/probe" || true)"
  [ "$code" = 200 ] || return 1
  got="$(signed "$S3/$bucket/probe" || true)"
  [ "$got" = "$body" ] || return 1
  signed -o /dev/null -X DELETE "$S3/$bucket/probe" || true
}

# Ready after 3 healthy round trips in a row, within RUSTFS_WAIT seconds (default 90).
wait_ready() {
  local ok=0 deadline=$((SECONDS + ${RUSTFS_WAIT:-90}))
  while [ "$SECONDS" -lt "$deadline" ]; do
    if healthy; then ok=$((ok + 1)); else ok=0; fi
    [ "$ok" -ge 3 ] && return 0
    sleep 1
  done
  echo "rustfs did not become healthy within ${RUSTFS_WAIT:-90}s" >&2
  docker logs --tail 200 "$NAME" >&2 || true
  return 1
}

case "${1:-}" in
  up)
    docker rm -f "$NAME" >/dev/null 2>&1 || true
    data="$(mktemp -d)"; chmod 777 "$data"
    docker run -d --name "$NAME" --restart on-failure:3 -p 127.0.0.1:9000:9000 \
      -e RUSTFS_ACCESS_KEY="$ACCESS" -e RUSTFS_SECRET_KEY="$SECRET" -e RUSTFS_CONSOLE_ENABLE=false \
      -v "$data:/data" "$IMAGE" /data >/dev/null
    wait_ready
    echo "export HSEARCH_S3_ENDPOINT=127.0.0.1:9000 HSEARCH_S3_ACCESS_KEY=$ACCESS HSEARCH_S3_SECRET_KEY=$SECRET HSEARCH_S3_REQUIRED=1 HSEARCH_S3_IMAGE=$IMAGE"
    ;;
  restart) docker restart "$NAME" >/dev/null; wait_ready ;;
  # Fails (after dumping the container log) when RustFS no longer answers a signed round trip;
  # CI runs it before each step that needs the store, so a dead store is named, not a flaky test.
  health) healthy || { echo "rustfs is not healthy" >&2; docker logs --tail 200 "$NAME" >&2 || true; exit 1; } ;;
  logs) docker logs "$NAME" 2>&1 ;;
  down) docker rm -f "$NAME" >/dev/null ;;
  *) echo "usage: $0 up|restart|health|logs|down" >&2; exit 2 ;;
esac
