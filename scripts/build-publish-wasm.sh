#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

# The artifact is vendored byte for byte by another repository, so a rebuild
# of the same commit must give the same bytes. That needs one fixed compiler
# and tool versions, and no machine-specific paths baked into the binary.
rust_version=1.100.0
bindgen_version=0.2.127
installed=$(wasm-bindgen --version)
if [[ "$installed" != "wasm-bindgen $bindgen_version" ]]; then
    printf 'Expected wasm-bindgen %s, found %s\n' "$bindgen_version" "$installed" >&2
    exit 1
fi
rustc_found=$(rustc --version)
if [[ "$rustc_found" != "rustc $rust_version "* ]]; then
    printf 'Expected rustc %s, found %s (set RUSTUP_TOOLCHAIN=%s)\n' \
        "$rust_version" "$rustc_found" "$rust_version" >&2
    exit 1
fi

# Panic and debug strings carry source paths. Remapping the checkout and the
# Cargo home makes them identical wherever the build runs.
repo_root=$(pwd -P)
cargo_home=${CARGO_HOME:-$HOME/.cargo}
export RUSTFLAGS="--remap-path-prefix=$repo_root=/fusiform --remap-path-prefix=$cargo_home=/cargo"

target=wasm32-unknown-unknown
profile=publish-wasm
target_dir=${CARGO_TARGET_DIR:-target}
out=dist/publish-wasm
wasm=fusiform_publish_wasm_bg.wasm
glue=fusiform_publish_wasm.js
commit=$(git rev-parse HEAD)

cargo build --locked -p fusiform-publish-wasm --target "$target" --profile "$profile" \
    --target-dir "$target_dir"
mkdir -p "$out"
rm -f "$out/SHA256SUMS"
wasm-bindgen --target web --no-typescript --out-dir "$out" \
    "$target_dir/$target/$profile/fusiform_publish_wasm.wasm"

optimizer='not run'
if command -v wasm-opt > /dev/null; then
    optimizer=$(wasm-opt --version)
    wasm-opt -Oz --all-features --strip-debug --strip-producers \
        "$out/$wasm" -o "$out/$wasm"
fi

raw_size=$(wc -c < "$out/$wasm")
gzip_size=$(gzip -9 -n -c "$out/$wasm" | wc -c)
{
    printf '# Fusiform publisher WebAssembly\n\n'
    printf -- '- Build commit: `%s`\n' "$commit"
    printf -- '- Compiler: `%s`\n' "$rustc_found"
    printf -- '- wasm-bindgen: `%s`\n- Optimizer: `%s`\n' "$bindgen_version" "$optimizer"
    printf -- '- WASM size: %d bytes raw, %d bytes gzip (`gzip -9 -n`)\n\n' "$raw_size" "$gzip_size"
    # Append the API documentation after the build-specific fields.
    cat crates/fusiform-publish-wasm/README.md
} > "$out/README.md"

(
    cd "$out"
    if command -v sha256sum > /dev/null; then
        sha256sum "$wasm" "$glue" README.md
    else
        shasum -a 256 "$wasm" "$glue" README.md
    fi
) > "$out/SHA256SUMS"
printf 'Wrote %s (%d bytes raw, %d bytes gzip)\n' "$out" "$raw_size" "$gzip_size"
