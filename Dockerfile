# OxideClaw container: the static musl release binary on a minimal Alpine base
# (git for worktrees and /undo, ca-certificates for the API, bash because the
# Bash tool runs `$SHELL -c` and falls back to `bash`; busybox sh would make
# every Bash tool call fail and the model writes bash syntax anyway; ripgrep
# because Grep's built-in fallback is slower and does not honour .gitignore).
#
#   docker run --rm -it --user "$(id -u):$(id -g)" -e HOME=/tmp \
#     -e ANTHROPIC_API_KEY -v "$PWD:/work" ghcr.io/forkedintime/oxideclaw
#
# A bind mount keeps the host owner, so --user is what makes /work writable
# when your uid is not 1000 (the image's `oxide` user); HOME=/tmp because a
# uid with no passwd entry gets HOME=/, where config and sessions can't be
# written. safe.directory below stops git from rejecting /work as "dubious
# ownership" if --user is left out, which would turn off /undo and autocommit.
#
# Build locally: docker build -t oxideclaw .
ARG OXIDECLAW_VERSION=0.4.0
FROM alpine:3.20 AS fetch
ARG OXIDECLAW_VERSION
RUN apk add --no-cache curl \
 && curl -fsSL -o /oxideclaw "https://github.com/ForkedInTime/OxideClaw/releases/download/v${OXIDECLAW_VERSION}/oxideclaw-linux-x64-musl" \
 && curl -fsSL -o /oxideclaw.sha256 "https://github.com/ForkedInTime/OxideClaw/releases/download/v${OXIDECLAW_VERSION}/oxideclaw-linux-x64-musl.sha256" \
 && echo "$(cut -d' ' -f1 /oxideclaw.sha256)  /oxideclaw" | sha256sum -c - \
 && chmod +x /oxideclaw

FROM alpine:3.20
RUN apk add --no-cache git ca-certificates bash ripgrep \
 && git config --system --add safe.directory '*' \
 && adduser -D -s /bin/bash -h /home/oxide oxide
ENV SHELL=/bin/bash
COPY --from=fetch /oxideclaw /usr/local/bin/oxideclaw
USER oxide
WORKDIR /work
ENTRYPOINT ["oxideclaw"]
