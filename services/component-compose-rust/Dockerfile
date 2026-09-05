# syntax=docker/dockerfile:1

# Multi-stage build for the aqw-component-compose Lambda.

# Build stage uses Amazon Linux 2023 (the same glibc 2.34 as the
# provided:al2023 runtime image), so the release binary is ABI-identical to
# the Lambda runtime and needs no musl cross-compilation. This worker runs
# only on Graviton (arm64); +neon enables tiny-skia SIMD.
FROM public.ecr.aws/lambda/provided:al2023 AS build
ARG TARGETARCH
RUN test "${TARGETARCH}" = "arm64" || (echo "component-compose-rust must be built for linux/arm64" >&2; exit 1)

# curl-minimal with /usr/bin/curl is already present in the al2023 base.
RUN dnf install -y \
      clang \
      cmake \
      gcc \
      gcc-c++ \
      git \
      make \
      perl \
    && dnf clean all \
    && rm -rf /var/cache/dnf

# Rust stable, pinned to match the toolchain that produced Cargo.lock.
ARG RUST_VERSION=1.98.0
RUN curl --fail --location --retry 3 https://sh.rustup.rs -sSf | \
      sh -s -- -y --profile minimal --default-toolchain ${RUST_VERSION} \
    && . "$HOME/.cargo/env" \
    && rustc --version \
    && cargo --version

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# Thin-LTO, single-CGU, stripped release profile for a fast cold start. arm64
# (Graviton) only: +neon for tiny-skia SIMD and the ring crate's NEON assert.
ENV CARGO_PROFILE_RELEASE_LTO="thin" \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS="1" \
    CARGO_PROFILE_RELEASE_STRIP="symbols" \
    CARGO_PROFILE_RELEASE_PANIC="abort"
RUN --mount=type=cache,id=aqw-component-compose-cargo-registry-arm64,target=/root/.cargo/registry,sharing=locked \
    --mount=type=cache,id=aqw-component-compose-cargo-git-arm64,target=/root/.cargo/git,sharing=locked \
    --mount=type=cache,id=aqw-component-compose-target-arm64,target=/build/target,sharing=locked \
    . "/root/.cargo/env" \
    && RUSTFLAGS="-C target-feature=+neon" cargo build --release --locked --bin aqw-component-compose \
    && mkdir -p /out \
    && cp target/release/aqw-component-compose /out/bootstrap

# ---- runtime image ---------------------------------------------------------
FROM public.ecr.aws/lambda/provided:al2023

ARG LIBWEBP_VERSION=1.5.0
ARG LIBWEBP_SHA256=ec874dd2e52097f11dd4999176ae6b6fb0ecb165bc1e05d3ecc9b29014fb003e

# The pinned cwebp binary only (aarch64 build for the Graviton worker),
# identical codec/flags to the Python renderer's x86-64 cwebp.
# curl (curl-minimal) ships with the al2023 base image; tar and gzip are
# added and removed here like the Python renderer image does.
RUN dnf install -y tar gzip \
    && curl --fail --location --retry 3 \
      "https://storage.googleapis.com/downloads.webmproject.org/releases/webp/libwebp-${LIBWEBP_VERSION}-linux-aarch64.tar.gz" \
      --output /tmp/libwebp.tar.gz \
    && echo "${LIBWEBP_SHA256}  /tmp/libwebp.tar.gz" | sha256sum --check --strict \
    && mkdir -p /opt/libwebp \
    && tar -xzf /tmp/libwebp.tar.gz --strip-components=1 -C /opt/libwebp \
    && rm /tmp/libwebp.tar.gz \
    && dnf remove -y tar gzip \
    && dnf clean all \
    && rm -rf /var/cache/dnf \
    && /opt/libwebp/bin/cwebp -version

COPY --from=build /out/bootstrap ${LAMBDA_RUNTIME_DIR}/bootstrap
RUN chmod +x ${LAMBDA_RUNTIME_DIR}/bootstrap

ENV CHAR_RENDER_CWEBP=/opt/libwebp/bin/cwebp \
    CHAR_RENDER_TEMP_DIR=/tmp

CMD ["bootstrap"]
