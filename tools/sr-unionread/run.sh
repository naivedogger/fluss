#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
umask 077
dc=(docker compose -f compose.yml)
case "${1:-help}" in
build)
  root=$(git rev-parse --show-toplevel)
  sr_rev=b47765ca9efac240e1844e35d25f86561a83d865
  docker buildx build --load --platform linux/amd64 \
    --build-context "fluss_src=$root/fluss-rust" \
    --build-context "sr_src=https://github.com/naivedogger/starrocks.git#$sr_rev" --build-arg "JOBS=${JOBS:-4}" \
    --build-arg "SR_BUILD_IMAGE=${SR_BUILD_IMAGE:-starrocks/dev-env-ubuntu:main-latest}" \
    --build-arg "RUST_IMAGE=${RUST_IMAGE:-rust:1.95.0-bookworm}" -t "${PROBE_IMAGE:-fluss-sr-ecs:local}" . ;;
up)
  test -f config.json || { echo 'Create config.json first.' >&2; exit 2; }
  chmod 600 config.json
  mkdir -p results
  "${dc[@]}" up -d fe be1 be2
  "${dc[@]}" run --rm --no-deps probe register ;;
peak)
  test -f config.json || { echo 'Create config.json first.' >&2; exit 2; }
  chmod 600 config.json
  shift
  "${dc[@]}" run --rm --no-deps --entrypoint python3 probe /opt/peak.py "$@" ;;
prepare|smoke|bench)
  test -f config.json || { echo 'Create config.json first.' >&2; exit 2; }
  chmod 600 config.json
  "${dc[@]}" run --rm --no-deps probe "$1" ;;
down) "${dc[@]}" down ;;
*) echo 'Usage: ./run.sh {build|up|prepare|smoke|bench|peak [start [max]]|down}' >&2; exit 2 ;;
esac
