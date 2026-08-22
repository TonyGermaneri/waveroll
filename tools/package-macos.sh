#!/usr/bin/env bash
# Builds the release configuration and packages one zip per plug-in format.
#
# Separate from the CI workflow on purpose: a release process that only exists inside a YAML file
# is one nobody can run before pushing a tag, and the first time it is exercised is the worst time
# to find out it does not work. Run it here, get the same zips.
#
#   tools/package-macos.sh [version]
#
# Leaves the artefacts in dist/. Does not install anything -- see WAVEROLL_INSTALL_AFTER_BUILD.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="${1:-$(git -C "$root" describe --tags --always --dirty 2>/dev/null || echo dev)}"
build="${WAVEROLL_BUILD_DIR:-$root/native/build-release}"
dist="$root/dist"

echo "==> Waveroll $version"

# Release, and universal. The dev tree is configured with no build type at all, which is a
# perfectly good default for iterating and a bad one to hand somebody: -O0 C++ around a -O3 Rust
# core, in a plug-in whose paint path runs at the display's refresh rate.
#
# The fetched JUCE is shared with any other build tree on this machine, so switching between the
# dev and release configurations does not re-clone it.
cmake -B "$build" -S "$root/native" -G Ninja \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_OSX_ARCHITECTURES="arm64;x86_64" \
  -DFETCHCONTENT_BASE_DIR="$root/native/.deps" \
  -DWAVEROLL_INSTALL_AFTER_BUILD=OFF

cmake --build "$build"

rm -rf "$dist/macos" && mkdir -p "$dist/macos"

# Found rather than assumed: JUCE puts artefacts under $<CONFIG>, which is an empty path segment
# when no build type is set and "Release" when one is, so the layout differs between the two trees
# this script and the dev build produce.
package() {
  local format="$1" bundle_name="$2"
  local bundle
  bundle="$(find "$build" -maxdepth 4 -name "$bundle_name" -print -quit)"
  [ -n "$bundle" ] || { echo "no $bundle_name under $build" >&2; exit 1; }

  local binary="$bundle/Contents/MacOS/Waveroll"
  local archs
  archs="$(lipo -archs "$binary")"
  [ "$archs" = "x86_64 arm64" ] || { echo "$format is $archs, not universal" >&2; exit 1; }

  # Ad-hoc, which is all an unsigned project can do and all arm64 requires to load at all. It is
  # not a Developer ID signature and does not clear the quarantine flag on a downloaded zip -- the
  # release notes tell people how to remove it.
  codesign --force --sign - --timestamp=none "$bundle"
  codesign --verify --strict "$bundle"

  # ditto, not zip: bundles carry symlinks and resource forks that zip(1) flattens, and a
  # flattened .component is one a host silently declines to load.
  ditto -c -k --sequesterRsrc --keepParent "$bundle" \
    "$dist/macos/Waveroll-$version-macOS-$format.zip"
  echo "    $format  $archs  $(du -h "$dist/macos/Waveroll-$version-macOS-$format.zip" | cut -f1)"
}

echo "==> Packaging"
package AU Waveroll.component
package VST3 Waveroll.vst3
package Standalone Waveroll.app

echo "==> dist/macos"
ls -1 "$dist/macos"
