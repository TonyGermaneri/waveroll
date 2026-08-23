#!/usr/bin/env bash
# Writes the body of a GitHub release to stdout.
#
#   tools/release-notes.sh <tag> [previous-tag]
#
# Here rather than inline in the workflow so it can be read before it is published.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tag="${1:?usage: release-notes.sh <tag> [previous-tag]}"
prev="${2:-$(git -C "$root" describe --tags --abbrev=0 "$tag^" 2>/dev/null || true)}"
repo="${GITHUB_REPOSITORY:-TonyGermaneri/waveroll}"

cat <<'EOF'
Waveroll records the audio and MIDI coming through it into a rolling buffer, and drags a
bar-quantised slice of that buffer straight into your DAW as a file.

## Install

Unzip and put the bundle where the host looks for it:

| Format | Goes in |
| --- | --- |
| `Waveroll.component` (AU) | `~/Library/Audio/Plug-Ins/Components` |
| `Waveroll.vst3` | `~/Library/Audio/Plug-Ins/VST3` |
| `Waveroll.app` | `/Applications`, or anywhere |
EOF

# What the notes may claim is what tools/package-macos.sh recorded doing, not what the workflow
# meant to do. A release that says "notarised" about an ad-hoc build is worse than one that says
# nothing, because the first thing it costs is somebody's trust in the rest of the page.
if [ "${WAVEROLL_SIGNING:-ad-hoc}" = notarised ]; then
cat <<'EOF'

Signed with a Developer ID, notarised by Apple, and stapled — so they install and load with no
warning, and with no network needed to check. Universal (arm64 and x86_64), macOS 13 and later.
EOF
else
cat <<'EOF'

These builds are **not signed with a Developer ID or notarised**, so macOS quarantines them on
download and a host will refuse to load one without saying why. Clear the flag after unzipping:

```sh
xattr -dr com.apple.quarantine ~/Library/Audio/Plug-Ins/Components/Waveroll.component
```

Universal (arm64 and x86_64), macOS 13 and later.
EOF
fi

cat <<'EOF'

The Audio Unit is validated with `auval` in the same job that built it, so it is the binary here
that passed, not one like it.

The web build is the same core compiled to WebAssembly. It captures and draws, but a browser
cannot hand a file to another application by drag -- it can only offer a promise, which no DAW
accepts -- so it writes into a folder instead. Unzip and serve it over HTTP (`python3 serve.py`
from the unzipped folder); opening `index.html` from disk will not work, because modules and
WebAssembly both need an origin.
EOF

if [ -n "$prev" ]; then
  printf '\n## Changes since %s\n\n' "$prev"
  git -C "$root" log --no-merges --format='- %s' "$prev..$tag"
  printf '\n[Full diff](https://github.com/%s/compare/%s...%s)\n' "$repo" "$prev" "$tag"
else
  printf '\n## Changes\n\nFirst release.\n'
fi

if [ -f "$root/dist/SHA256SUMS.txt" ]; then
  printf '\n## Checksums\n\n```\n'
  cat "$root/dist/SHA256SUMS.txt"
  printf '```\n'
fi
