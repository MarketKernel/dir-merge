#!/usr/bin/env bash
# Quick build for DirMerge.
#
#   ./build_scripts/build.sh            release build
#   ./build_scripts/build.sh --debug    debug build (faster to compile)
#   ./build_scripts/build.sh --test     run tests, then release build
#   ./build_scripts/build.sh --run [paths...]   build and launch (e.g. two files/folders)
set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found — install Rust from https://rustup.rs" >&2
    exit 1
fi

profile="release"
run=false
test=false
args=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --debug) profile="debug" ;;
        --test) test=true ;;
        --run) run=true; shift; args=("$@"); break ;;
        -h|--help) sed -n '2,7p' "$0"; exit 0 ;;
        *) echo "unknown option: $1 (see --help)" >&2; exit 1 ;;
    esac
    shift
done

if $test; then
    cargo test --release
fi

start=$(date +%s)
if [[ "$profile" == "release" ]]; then
    cargo build --release
else
    cargo build
fi

bin="target/$profile/dir-merge"
[[ "${OS:-}" == "Windows_NT" ]] && bin="$bin.exe"
echo "✔ built $bin ($(( $(date +%s) - start ))s)"

if $run; then
    exec "$bin" "${args[@]+"${args[@]}"}"
fi
