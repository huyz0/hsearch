#!/usr/bin/env bash
# Start the digest-pinned RustFS from dev/images.json on localhost:9000 and wait until it
# answers. Prints `export` lines for the S3 test environment. Local use and CI:
#   eval "$(scripts/rustfs.sh up)"   ...   scripts/rustfs.sh restart   ...   scripts/rustfs.sh down
set -euo pipefail
cd "$(dirname "$0")/.."
NAME="${RUSTFS_NAME:-hsearch-rustfs}"
IMAGE="$(python3 -c 'import json;print(json.load(open("dev/images.json"))["rustfs"])')"
ACCESS=hsearchtest
SECRET=hsearchtestsecret

wait_ready() {
  for _ in $(seq 1 60); do
    # Any HTTP answer, even 403, means the S3 listener is up.
    if [ "$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 http://127.0.0.1:9000/ || true)" != "000" ]; then return 0; fi
    sleep 1
  done
  echo "rustfs did not become ready" >&2
  docker logs "$NAME" >&2 || true
  return 1
}

case "${1:-}" in
  up)
    docker rm -f "$NAME" >/dev/null 2>&1 || true
    data="$(mktemp -d)"; chmod 777 "$data"
    docker run -d --name "$NAME" -p 127.0.0.1:9000:9000 \
      -e RUSTFS_ACCESS_KEY="$ACCESS" -e RUSTFS_SECRET_KEY="$SECRET" -e RUSTFS_CONSOLE_ENABLE=false \
      -v "$data:/data" "$IMAGE" /data >/dev/null
    wait_ready
    echo "export HSEARCH_S3_ENDPOINT=127.0.0.1:9000 HSEARCH_S3_ACCESS_KEY=$ACCESS HSEARCH_S3_SECRET_KEY=$SECRET HSEARCH_S3_REQUIRED=1 HSEARCH_S3_IMAGE=$IMAGE"
    ;;
  restart) docker restart "$NAME" >/dev/null; wait_ready ;;
  down) docker rm -f "$NAME" >/dev/null ;;
  *) echo "usage: $0 up|restart|down" >&2; exit 2 ;;
esac
