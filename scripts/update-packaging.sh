#!/usr/bin/env bash
# update-packaging.sh — refresh contrib/aur/PKGBUILD (+ .SRCINFO) and
# contrib/homebrew/oxideclaw.rb to a released tag, using the per-asset
# SHA-256 files the release workflow publishes.
#
#   scripts/update-packaging.sh v0.3.3
#
# Then: push contrib/homebrew/oxideclaw.rb to the tap repo and
# contrib/aur/{PKGBUILD,.SRCINFO} to the AUR (see docs in each file).
set -euo pipefail

tag="${1:?usage: $0 vX.Y.Z}"
ver="${tag#v}"
repo="ForkedInTime/OxideClaw"
root="$(cd "$(dirname "$0")/.." && pwd)"

sum() {
  curl -fsSL "https://github.com/${repo}/releases/download/${tag}/$1.sha256" | cut -d' ' -f1
}

lx=$(sum oxideclaw-linux-x64)
la=$(sum oxideclaw-linux-arm64)
mx=$(sum oxideclaw-macos-x64)
ma=$(sum oxideclaw-macos-arm64)

pkg="${root}/contrib/aur/PKGBUILD"
sed -i -E "s/^pkgver=.*/pkgver=${ver}/; s/^pkgrel=.*/pkgrel=1/" "$pkg"
sed -i -E "s/^sha256sums_x86_64=.*/sha256sums_x86_64=('${lx}')/; s/^sha256sums_aarch64=.*/sha256sums_aarch64=('${la}')/" "$pkg"
if command -v makepkg >/dev/null; then
  (cd "${root}/contrib/aur" && makepkg --printsrcinfo > .SRCINFO)
else
  echo "makepkg not found: regenerate contrib/aur/.SRCINFO on an Arch host" >&2
fi

rb="${root}/contrib/homebrew/oxideclaw.rb"
sed -i -E "s/^  version \".*\"/  version \"${ver}\"/" "$rb"
# Order in the file: macos arm, macos intel, linux arm, linux intel.
python3 - "$rb" "$ma" "$mx" "$la" "$lx" <<'PY'
import re, sys
p, *sums = sys.argv[1:]
s = open(p).read()
it = iter(sums)
s = re.sub(r'sha256 "[0-9a-f]{64}"', lambda m: f'sha256 "{next(it)}"', s)
open(p, "w").write(s)
PY

echo "packaging files now point at ${tag}"
grep -E '^(pkgver|sha256sums)' "$pkg"
grep -E 'version|sha256' "$rb"
