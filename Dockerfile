# OxideClaw container: the static musl release binary on a minimal Alpine base
# (git for worktrees and /undo, ca-certificates for the API).
#
#   docker run --rm -it -e ANTHROPIC_API_KEY -v "$PWD:/work" ghcr.io/forkedintime/oxideclaw
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
RUN apk add --no-cache git ca-certificates \
 && adduser -D -h /home/oxide oxide
COPY --from=fetch /oxideclaw /usr/local/bin/oxideclaw
USER oxide
WORKDIR /work
ENTRYPOINT ["oxideclaw"]
