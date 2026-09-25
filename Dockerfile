# syntax=docker/dockerfile:1
#
# Assistant Gateway -- multi-arch, static-musl build.
#
# Every architecture is cross-compiled from ONE native rustc using
# cargo-zigbuild (zig is the cross C compiler/linker). No QEMU and no per-arch
# base image -- the builder always runs on $BUILDPLATFORM and only the Rust
# target triple changes. Same recipe as doover-tunnels / doover-app-controller.
#
# The final image is Alpine rather than scratch: host commands run the host's
# own /bin/sh (the binary joins PID 1's namespaces itself, no nsenter needed),
# but container-mode commands (`exec` with `where: "container"`, and
# `scan_network`) run in here, and need a shell plus the diagnostic tools
# (nmap, arp-scan, nmcli, mbpoll, ...). Those tools come from Alpine packages,
# installed per target arch -- CI sets up QEMU, so `RUN apk add` works for
# every platform. mbpoll isn't packaged, so it's built from source in its own
# (target-arch) stage against libmodbus.
#
# Build one arch locally:
#   docker buildx build --platform linux/arm64 -t assistant-gateway:local --load .

ARG ZIG_VERSION=0.13.0

## BUILD STAGE (runs on $BUILDPLATFORM, cross-compiles to $TARGETPLATFORM) ##
FROM --platform=$BUILDPLATFORM rust:1-bookworm AS builder
ARG ZIG_VERSION
ARG TARGETPLATFORM
ARG TARGETARCH
ARG TARGETVARIANT

# NB: no system protoc -- doover-proto's build.rs falls back to its vendored
# protoc when PROTOC is unset.
RUN apt-get update && apt-get install -y --no-install-recommends \
        xz-utils curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN set -eux; \
    case "$(uname -m)" in \
        aarch64) ZARCH=aarch64 ;; \
        x86_64)  ZARCH=x86_64  ;; \
        *) echo "unsupported build arch $(uname -m)" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-linux-${ZARCH}-${ZIG_VERSION}.tar.xz" -o /tmp/zig.tar.xz; \
    mkdir -p /opt/zig; tar -xJf /tmp/zig.tar.xz -C /opt/zig --strip-components=1; \
    ln -s /opt/zig/zig /usr/local/bin/zig; \
    zig version
RUN cargo install cargo-zigbuild --locked

RUN set -eux; \
    case "$TARGETPLATFORM" in \
        linux/amd64)  TRIPLE=x86_64-unknown-linux-musl      ;; \
        linux/arm64)  TRIPLE=aarch64-unknown-linux-musl     ;; \
        linux/arm/v7) TRIPLE=armv7-unknown-linux-musleabihf ;; \
        linux/arm/v6) TRIPLE=arm-unknown-linux-musleabihf   ;; \
        *) echo "unsupported target platform: $TARGETPLATFORM" >&2; exit 1 ;; \
    esac; \
    echo "$TRIPLE" > /tmp/triple; \
    rustup target add "$TRIPLE"

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ ./src/
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=ag-registry-${TARGETARCH}${TARGETVARIANT} \
    --mount=type=cache,target=/build/target,id=ag-target-${TARGETARCH}${TARGETVARIANT} \
    set -eux; \
    TRIPLE="$(cat /tmp/triple)"; \
    cargo zigbuild --release --locked --target "$TRIPLE"; \
    cp "target/${TRIPLE}/release/assistant-gateway" /assistant-gateway

## MBPOLL STAGE (runs on $TARGETPLATFORM) ##
FROM alpine:3 AS mbpoll
ARG MBPOLL_VERSION=1.5.4
# A tagged git clone rather than the release tarball: mbpoll's CMake takes its
# version from `git describe` (run in cmake's cwd, hence the `cd`), and
# reports 1.0-0 without it.
RUN apk add --no-cache build-base cmake git libmodbus-dev pkgconf
RUN set -eux; \
    git clone --depth 1 --branch "v${MBPOLL_VERSION}" https://github.com/epsilonrt/mbpoll.git /tmp/mbpoll; \
    cd /tmp/mbpoll; \
    cmake -S . -B build -DCMAKE_BUILD_TYPE=Release; \
    cmake --build build -j"$(nproc)"; \
    install -m 0755 "$(find build -type f -name mbpoll -perm -u+x | head -1)" /usr/local/bin/mbpoll; \
    /usr/local/bin/mbpoll -V

## FINAL IMAGE (runs on $TARGETPLATFORM) ##
FROM alpine:3 AS final_image
LABEL com.doover.app="true"
LABEL com.doover.managed="true"

# Diagnostic tools for container-mode commands. busybox (in the base image)
# still provides wget for the HEALTHCHECK. iproute2 because busybox `ip` has
# no `-j`. libmodbus is mbpoll's runtime library.
RUN apk add --no-cache \
        networkmanager-cli nmap arp-scan socat iputils iproute2 curl jq \
        busybox-extras ca-certificates libmodbus

COPY --from=mbpoll /usr/local/bin/mbpoll /usr/local/bin/mbpoll
COPY --from=builder /assistant-gateway /assistant-gateway

HEALTHCHECK --interval=30s --timeout=2s --start-period=5s \
    CMD wget -q -O /dev/null "http://127.0.0.1:$HEALTHCHECK_PORT" || exit 1

STOPSIGNAL SIGTERM
ENTRYPOINT ["/assistant-gateway"]
