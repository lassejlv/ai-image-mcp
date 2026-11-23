# syntax=docker/dockerfile:1

# Cross-compiles on the build machine's own architecture for each target platform,
# so multi-arch builds need no emulation.
FROM --platform=$BUILDPLATFORM rust:latest AS build
ARG TARGETARCH
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-gnu; arch=x86-64; libc=amd64 ;; \
      arm64) target=aarch64-unknown-linux-gnu; arch=aarch64; libc=arm64 ;; \
      *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    if [ "$(dpkg --print-architecture)" != "$TARGETARCH" ]; then \
      apt-get update; \
      apt-get install -y --no-install-recommends "gcc-$arch-linux-gnu" "libc6-dev-$libc-cross"; \
      rm -rf /var/lib/apt/lists/*; \
    fi; \
    rustup target add "$target"; \
    echo "$target" > /rust-target
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=target-$TARGETARCH \
    target="$(cat /rust-target)" \
    && cargo build --release --locked --target "$target" \
    && cp "target/$target/release/ai-image-mcp" /ai-image-mcp

FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /ai-image-mcp /usr/local/bin/ai-image-mcp
# Config and saved logins; mount a volume here to keep them.
ENV AI_IMAGE_MCP_HOME=/data
WORKDIR /data
ENTRYPOINT ["ai-image-mcp"]
CMD ["serve"]
