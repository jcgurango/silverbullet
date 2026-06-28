# syntax=docker/dockerfile:1
#
# Self-contained multi-stage build for the SilverBullet server.
#
# Stage 1 (builder): rust:alpine — installs Node.js + build deps, runs
#   `npm run build` to produce the client bundle + `version.json`, then
#   `cargo build --release` for the server. The Rust binary embeds the client
#   bundle via rust-embed at compile time, so the npm step MUST come first.
#
# Stage 2 (runtime): bare alpine + tini + the binary.
#
# `docker build .` now Just Works without pre-compiled artifacts. CI's
# pre-built-binary fast path lives in `Dockerfile.ci` for multi-arch buildx
# where in-image compilation under QEMU emulation would be too slow.
#
# This is the BASE variant: no Chromium, so `/.runtime/*` returns 503.
# `Dockerfile.runtime-api` layers Chromium on top.
#
# Published by `.github/workflows/ci.yml`.

FROM rust:alpine AS builder

# build-base = gcc/musl-dev/make for crates with native build scripts (e.g. ring).
# nodejs/npm for the client bundle build. git for `git describe` in version.ts.
RUN apk add --no-cache nodejs npm git build-base pkgconfig

# The repo's .cargo/config.toml hardwires `musl-gcc` / `aarch64-linux-gnu-gcc`
# / `arm-linux-gnueabihf-gcc` as the per-target linker (CI cross-compiles from
# Debian where those are the apt-installed cross toolchains). On Alpine the
# native `gcc` IS musl-gcc, so override the config via env so we link with the
# host compiler regardless of which arch buildx is targeting.
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=gcc \
    CC_x86_64_unknown_linux_musl=gcc \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=gcc \
    CC_aarch64_unknown_linux_musl=gcc \
    AR_aarch64_unknown_linux_musl=ar \
    CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=gcc \
    CC_armv7_unknown_linux_musleabihf=gcc \
    AR_armv7_unknown_linux_musleabihf=ar

WORKDIR /src
COPY . .

# Cache mounts speed up rebuilds dramatically (with BuildKit, on by default in
# modern docker). The caches are NOT baked into the final image.
RUN --mount=type=cache,target=/root/.npm \
    npm ci

# Builds plugs + client bundle + writes version.json (build_plugs.ts calls
# updateVersionFile()). Must run before the cargo build below — the Rust crate
# embeds client_bundle/ at compile time.
RUN npm run build

RUN cargo build --release -p silverbullet \
    && cp /src/target/release/silverbullet /silverbullet

FROM alpine:latest

RUN apk add --no-cache git curl bash tini

ENV SB_HOSTNAME=0.0.0.0 \
    SB_FOLDER=/space \
    SB_PORT=3000

EXPOSE 3000
HEALTHCHECK CMD curl --fail "http://localhost:$SB_PORT$SB_URL_PREFIX/.ping" || exit 1

COPY --from=builder /silverbullet /silverbullet
RUN chmod +x /silverbullet

# Extra args (e.g. `--user me:letmein`) are appended to the binary invocation.
ENTRYPOINT ["/sbin/tini", "--", "/silverbullet"]
