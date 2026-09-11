# Homebrew formula

`rustyclaw.rb` is the source of truth for the tap
[ForkedInTime/homebrew-rustyclaw](https://github.com/ForkedInTime/homebrew-rustyclaw).

Per release:

```bash
scripts/update-packaging.sh vX.Y.Z                 # bumps version + sha256s
git clone git@github.com:ForkedInTime/homebrew-rustyclaw.git /tmp/tap
cp contrib/homebrew/rustyclaw.rb /tmp/tap/Formula/rustyclaw.rb
cd /tmp/tap && git commit -am "rustyclaw vX.Y.Z" && git push
```

Users: `brew install ForkedInTime/rustyclaw/rustyclaw`.
