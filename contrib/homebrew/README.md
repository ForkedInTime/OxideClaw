# Homebrew formula

`oxideclaw.rb` is the source of truth for the tap
[ForkedInTime/homebrew-oxideclaw](https://github.com/ForkedInTime/homebrew-oxideclaw).

Per release:

```bash
scripts/update-packaging.sh vX.Y.Z                 # bumps version + sha256s
git clone git@github.com:ForkedInTime/homebrew-oxideclaw.git /tmp/tap
cp contrib/homebrew/oxideclaw.rb /tmp/tap/Formula/oxideclaw.rb
cd /tmp/tap && git commit -am "oxideclaw vX.Y.Z" && git push
```

Users: `brew install ForkedInTime/oxideclaw/oxideclaw`.
