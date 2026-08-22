#!/usr/bin/env bash
# Builds the browser target and assembles the static site that ships with a release and gets
# published to Pages.
#
#   tools/package-web.sh [version]
#
# Leaves the site in dist/web/ and a zip of it in dist/.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="${1:-$(git -C "$root" describe --tags --always --dirty 2>/dev/null || echo dev)}"
dist="$root/dist"

echo "==> Waveroll $version (web)"

# --release, because the default is a debug wasm build: several times the size, and slow enough
# that the paint path misses frames.
wasm-pack build "$root/crates/waveroll-wasm" \
  --release --target web --out-dir "$root/web/pkg"

site="$dist/web"
rm -rf "$site" && mkdir -p "$site"
cp "$root/web/index.html" "$site/"
# Shipped with the site because the release notes tell people to run it: the page needs an origin,
# and this is the smallest thing that provides one.
cp "$root/web/serve.py" "$site/"
cp -R "$root/web/src" "$site/src"
mkdir -p "$site/pkg"
# Only what the page loads. The .d.ts files and package.json are for a bundler this project does
# not use, and the .gitignore would be published as a file.
cp "$root/web/pkg/waveroll_wasm.js" "$root/web/pkg/waveroll_wasm_bg.wasm" "$site/pkg/"

# Pages serves through Jekyll unless told otherwise, and Jekyll drops paths beginning with an
# underscore. Nothing here starts with one today; this costs nothing and removes the trap.
touch "$site/.nojekyll"

( cd "$dist" && zip -qr "Waveroll-$version-web.zip" web )

echo "==> dist"
echo "    web/           $(du -sh "$site" | cut -f1)"
echo "    $(basename "$dist/Waveroll-$version-web.zip")  $(du -h "$dist/Waveroll-$version-web.zip" | cut -f1)"
