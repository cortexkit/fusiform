#!/usr/bin/env bash
# Install missing publisher tools into the caller's job-local bin directory.
set -euo pipefail

tools=${1:?usage: install-publish-wasm-tools.sh BIN_DIR}
mkdir -p "$tools"
tools=$(cd "$tools" && pwd)
export PATH="$tools:$PATH"

fetch() {
    local url=$1 archive=$2 checksum=$3
    curl -fsSLo "$tools/$archive" "$url/$archive"
    (cd "$tools" && printf '%s  %s\n' "$checksum" "$archive" | sha256sum -c -)
}

if ! bindgen_found=$(wasm-bindgen --version 2>/dev/null) || [[ "$bindgen_found" != "wasm-bindgen 0.2.127" ]]; then
    bindgen=wasm-bindgen-0.2.127-x86_64-unknown-linux-musl
    fetch https://github.com/wasm-bindgen/wasm-bindgen/releases/download/0.2.127 "$bindgen.tar.gz" \
        61d4a7dc85acfa0d2354ccc0b8361928c7e52a746d17f28ebaa795ed3dc1614a
    tar -xzf "$tools/$bindgen.tar.gz" -C "$tools" --strip-components=1 "$bindgen/wasm-bindgen"
    rm "$tools/$bindgen.tar.gz"
fi
if ! optimizer=$(wasm-opt --version 2>/dev/null) || \
   [[ "$optimizer" != 'wasm-opt version 120' && "$optimizer" != 'wasm-opt version 120 '* ]]; then
    binaryen=binaryen-version_120-x86_64-linux
    fetch https://github.com/WebAssembly/binaryen/releases/download/version_120 "$binaryen.tar.gz" \
        ddb097af51d1bdb17300d986b0de7d97422f1933dedb4c9eda3510e0bf4076cc
    tar -xzf "$tools/$binaryen.tar.gz" -C "$tools" --strip-components=2 binaryen-version_120/bin/wasm-opt
    rm "$tools/$binaryen.tar.gz"
fi
