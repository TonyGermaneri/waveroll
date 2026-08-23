#!/usr/bin/env bash
# Builds the release configuration, signs it, and packages one zip per plug-in format.
#
# Separate from the CI workflow on purpose: a release process that only exists inside a YAML file
# is one nobody can run before pushing a tag, and the first time it is exercised is the worst time
# to find out it does not work. Run it here, get the same zips.
#
#   tools/package-macos.sh [version]
#
# Signing is decided by what is available, so the same script covers a laptop and a runner:
#
#   WAVEROLL_SIGN_IDENTITY   a codesign identity, or "-" to force ad-hoc; otherwise the one
#                            Developer ID Application identity in the keychain, otherwise ad-hoc
#   WAVEROLL_NOTARIZE        auto (default) | required | off
#   NOTARY_KEY_PATH       App Store Connect .p8, with NOTARY_KEY_ID and NOTARY_ISSUER_ID
#
# `required` is what a release passes, so a missing credential stops the build rather than quietly
# shipping something the release notes claim is notarised.
#
# Leaves the artefacts in dist/. Does not install anything -- see WAVEROLL_INSTALL_AFTER_BUILD.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="${1:-$(git -C "$root" describe --tags --always --dirty 2>/dev/null || echo dev)}"
build="${WAVEROLL_BUILD_DIR:-$root/native/build-release}"
dist="$root/dist"
notarize="${WAVEROLL_NOTARIZE:-auto}"

echo "==> Waveroll $version"

# ---------------------------------------------------------------- what can we sign with, and how

identity="${WAVEROLL_SIGN_IDENTITY:-}"
# codesign's own spelling for ad-hoc, so a machine that has an identity can still be asked not to
# use it -- which is what CI does, and what makes an unsigned build reproducible anywhere.
[ "$identity" = "-" ] && identity=""
if [ -z "${identity}" ] && [ "${WAVEROLL_SIGN_IDENTITY:-}" != "-" ]; then
  # sed over the quoted name, because the identity is what codesign wants and the hash in front of
  # it is not. An empty list is not an error here: ad-hoc is a legitimate outcome.
  found="$(security find-identity -v -p codesigning 2>/dev/null \
    | grep 'Developer ID Application' | sed -E 's/^.*"(.*)".*$/\1/' || true)"
  count="$(printf '%s' "$found" | grep -c . || true)"
  case "$count" in
    0) ;;
    1) identity="$found" ;;
    *) echo "more than one Developer ID Application identity; set WAVEROLL_SIGN_IDENTITY" >&2
       printf '%s\n' "$found" >&2; exit 1 ;;
  esac
fi

have_creds=0
if [ -n "${NOTARY_KEY_PATH:-}" ] && [ -n "${NOTARY_KEY_ID:-}" ] \
   && [ -n "${NOTARY_ISSUER_ID:-}" ] && [ -f "${NOTARY_KEY_PATH:-}" ]; then
  have_creds=1
fi

case "$notarize" in
  required)
    [ -n "$identity" ] || { echo "notarisation required, but no Developer ID identity" >&2; exit 1; }
    [ "$have_creds" = 1 ] || { echo "notarisation required, but no App Store Connect key" >&2; exit 1; }
    do_notarize=1 ;;
  off)  do_notarize=0 ;;
  auto) if [ -n "$identity" ] && [ "$have_creds" = 1 ]; then do_notarize=1; else do_notarize=0; fi ;;
  *)    echo "WAVEROLL_NOTARIZE must be auto, required or off" >&2; exit 1 ;;
esac

if [ -n "$identity" ]; then
  echo "    signing as $identity"
else
  # All arm64 needs to load at all, and all an unsigned project can do. It does not clear the
  # quarantine flag on a downloaded zip, which is why a release passes WAVEROLL_NOTARIZE=required.
  echo "    signing ad-hoc: no Developer ID identity found"
fi
[ "$do_notarize" = 1 ] && echo "    notarising" || echo "    not notarising"

# --------------------------------------------------------------------------------------- build

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

# ------------------------------------------------------------------------------- sign and zip

bundles=()
zips=()

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

  local args=(--force)
  if [ -n "$identity" ]; then
    # --options runtime because notarisation refuses anything else, and --timestamp because a
    # signature without one stops verifying the day the certificate expires.
    args+=(--options runtime --timestamp --sign "$identity")
    # Only the app: a plug-in's entitlements are ignored, since it is the host process that owns
    # the entitlements of the process the plug-in runs in.
    [ "$bundle_name" = "Waveroll.app" ] && args+=(--entitlements "$root/native/Waveroll.entitlements")
  else
    args+=(--sign - --timestamp=none)
  fi
  codesign "${args[@]}" "$bundle"
  codesign --verify --strict "$bundle"

  local zip="$dist/macos/Waveroll-$version-macOS-$format.zip"
  # ditto, not zip: bundles carry symlinks and resource forks that zip(1) flattens, and a
  # flattened .component is one a host silently declines to load.
  ditto -c -k --sequesterRsrc --keepParent "$bundle" "$zip"
  bundles+=("$bundle")
  zips+=("$zip")
  echo "    $format  $archs  $(du -h "$zip" | cut -f1)"
}

echo "==> Packaging"
package AU Waveroll.component
package VST3 Waveroll.vst3
package Standalone Waveroll.app

# ------------------------------------------------------------------------------ notarise, staple

if [ "$do_notarize" = 1 ]; then
  nt() { xcrun notarytool "$@" \
    --key "$NOTARY_KEY_PATH" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER_ID"; }

  # Submitted together and waited on afterwards rather than one --wait at a time: Apple queues
  # these, and three sequential waits is three queue times instead of one.
  echo "==> Submitting to Apple"
  ids=()
  for zip in "${zips[@]}"; do
    id="$(nt submit "$zip" --output-format json | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')"
    echo "    $(basename "$zip")  $id"
    ids+=("$id")
  done

  echo "==> Waiting"
  for i in "${!ids[@]}"; do
    status="$(nt wait "${ids[$i]}" --timeout 30m --output-format json \
      | python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])')"
    echo "    $(basename "${zips[$i]}")  $status"
    if [ "$status" != "Accepted" ]; then
      # The log is the only thing that says which file was rejected and why. Printing it is the
      # difference between a fixable failure and a mystery.
      nt log "${ids[$i]}" || true
      exit 1
    fi
  done

  # Stapled to the bundle, not the zip, so the ticket travels with the installed plug-in and
  # Gatekeeper can clear it with no network. Then re-zipped, because the zip that was submitted
  # was made before the ticket existed.
  echo "==> Stapling"
  for i in "${!bundles[@]}"; do
    xcrun stapler staple "${bundles[$i]}"
    rm -f "${zips[$i]}"
    ditto -c -k --sequesterRsrc --keepParent "${bundles[$i]}" "${zips[$i]}"
    xcrun stapler validate "${bundles[$i]}"
  done
fi

# Read by tools/release-notes.sh, so the notes describe the build that was actually made rather
# than the one the workflow hoped for. Not a zip, so it is not attached to the release.
if [ "$do_notarize" = 1 ]; then echo notarised > "$dist/macos/signing.txt"
else echo ad-hoc > "$dist/macos/signing.txt"; fi

echo "==> dist/macos"
ls -1 "$dist/macos"
