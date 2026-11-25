#!/usr/bin/env bash
# Same publisher checks for CI and the Linux gate. The caller provides Node and
# the pinned tools checked by the build script.
set -euo pipefail
cd "$(dirname "$0")/.."

bash scripts/build-publish-wasm.sh
bash crates/fusiform-publish-wasm/tests/build-tools.sh
node --test crates/fusiform-publish-wasm/tests/node.mjs
(cd dist/publish-wasm && sha256sum -c SHA256SUMS)
