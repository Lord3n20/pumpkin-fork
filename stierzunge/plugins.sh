#!/usr/bin/env bash
# Builds the WASM plugins for this fork against the plugin API in this very tree,
# so the API version always matches the server binary.
# Output: stierzunge/dist/plugins/*.wasm
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$ROOT/target/stierzunge-plugins"
OUT="$ROOT/stierzunge/dist/plugins"
API="$ROOT/crates/pumpkin-plugin-api"
mkdir -p "$WORK" "$OUT"

# name | repo | pinned commit
PLUGINS=(
  "pumpkin_voice|https://github.com/hmdnnrmn/PumpkinVoice.git|0f9edf3387d20ab584e0cdcc4f1e4d9538bfdbce"
  "pumpkin_pregen|https://github.com/Thijs226/pumpkin-world-pregen.git|9081d4ee63caaa1290a42030f90feb9dab730346"
)

for entry in "${PLUGINS[@]}"; do
  IFS='|' read -r name repo rev <<<"$entry"
  dir="$WORK/$name"
  if [ ! -d "$dir/.git" ]; then
    git clone -q "$repo" "$dir"
  fi
  git -C "$dir" fetch -q origin
  git -C "$dir" checkout -q --force "$rev"
  git -C "$dir" clean -qfdx -e target
  # Our own changes to the plugin, if any (stierzunge/<name>*.patch)
  for patch in "$ROOT"/stierzunge/"${name#pumpkin_}"-*.patch; do
    [ -e "$patch" ] && git -C "$dir" apply "$patch"
  done

  # Point the plugin at the API crate of this tree instead of a git rev or crates.io version
  sed -i -E "s|^pumpkin-plugin-api = .*|pumpkin-plugin-api = { path = \"$API\" }|" "$dir/Cargo.toml"
  # The checkout lives inside the Pumpkin workspace folder; keep it a workspace of its own
  grep -q '^\[workspace\]' "$dir/Cargo.toml" || printf '\n[workspace]\n' >>"$dir/Cargo.toml"
  rm -f "$dir/Cargo.lock"

  (cd "$dir" && cargo build --release --target wasm32-wasip2)
  cp "$dir/target/wasm32-wasip2/release/$name.wasm" "$OUT/"
  echo "built $OUT/$name.wasm ($rev)"
done
