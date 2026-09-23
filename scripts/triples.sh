#!/usr/bin/env bash
# triple 名称 ↔ Rust target 的单一映射表。打包脚本与 CI 都从这里取，避免两处漂移。
set -eu

triple_to_target() {
  case "$1" in
    darwin-arm64) echo "aarch64-apple-darwin" ;;
    darwin-x64)   echo "x86_64-apple-darwin" ;;
    linux-x64)    echo "x86_64-unknown-linux-musl" ;;
    linux-arm64)  echo "aarch64-unknown-linux-musl" ;;
    *) return 1 ;;
  esac
}

host_triple() {
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64)  echo "darwin-arm64" ;;
    Darwin-x86_64) echo "darwin-x64" ;;
    Linux-x86_64)  echo "linux-x64" ;;
    Linux-aarch64) echo "linux-arm64" ;;
    *) return 1 ;;
  esac
}

supported_triples() { echo "darwin-arm64 darwin-x64 linux-x64 linux-arm64"; }
