# AUR package: `oxideclaw-bin`

Prebuilt binary package for Arch Linux. `PKGBUILD` downloads the release
asset for the host architecture, verifies it against the SHA-256 the
release workflow publishes, installs `/usr/bin/oxideclaw`, and generates
bash/zsh/fish completions from the binary.

## Refresh for a new release

```bash
scripts/update-packaging.sh vX.Y.Z      # rewrites pkgver + checksums + .SRCINFO
cd contrib/aur && makepkg -f --noconfirm  # local build test
```

## Publish to the AUR (maintainer, one-time setup + per release)

```bash
# one-time: AUR account with your SSH key, then
git clone ssh://aur@aur.archlinux.org/oxideclaw-bin.git /tmp/oxideclaw-bin

# per release
cp contrib/aur/PKGBUILD contrib/aur/.SRCINFO /tmp/oxideclaw-bin/
cd /tmp/oxideclaw-bin && git add PKGBUILD .SRCINFO && git commit -m "Update to vX.Y.Z" && git push
```

Users then install with `yay -S oxideclaw-bin` (or any AUR helper).
