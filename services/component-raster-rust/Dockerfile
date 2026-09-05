# syntax=docker/dockerfile:1

# Multi-stage build for the aqw-component-raster Lambda.
#
# Builds inside `provided:al2023` (the same glibc 2.34 as the runtime). This
# worker runs only on Graviton (arm64); +neon enables tiny-skia SIMD and the
# ring crate's compile-time NEON assertion. resvg is linked in-process, so the
# release bootstrap is a single lean ELF with no resvg CLI process.

FROM public.ecr.aws/lambda/provided:al2023 AS build
ARG TARGETARCH
RUN test "${TARGETARCH}" = "arm64" || (echo "component-raster-rust must be built for linux/arm64" >&2; exit 1)

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

# Rust stable, pinned to match the toolchain that produced Cargo.lock. The
# production build uses the crate's default resvg-only feature set, so the
# optional vendored ThorVG C++ backend is not compiled or linked.
ARG RUST_VERSION=1.98.0
RUN curl --fail --location --retry 3 https://sh.rustup.rs -sSf | \
      sh -s -- -y --profile minimal --default-toolchain ${RUST_VERSION} \
    && . "$HOME/.cargo/env" \
    && rustc --version \
    && cargo --version

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY vendor ./vendor

# Thin-LTO, single-CGU, stripped release profile for a fast cold start.
# ARM (Graviton) is the only deployed architecture; +neon enables tiny-skia
# SIMD and satisfies the ring crate's compile-time NEON assertion. A v3-vs-v2
# x86 A/B was slower and AVX did not help the scalar blur that dominates wing
# frames, so no x86 branch is retained.

ENV CARGO_PROFILE_RELEASE_LTO="thin" \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS="1" \
    CARGO_PROFILE_RELEASE_STRIP="symbols" \
    CARGO_PROFILE_RELEASE_PANIC="abort"
RUN --mount=type=cache,id=aqw-component-raster-cargo-registry-arm64,target=/root/.cargo/registry,sharing=locked \
    --mount=type=cache,id=aqw-component-raster-cargo-git-arm64,target=/root/.cargo/git,sharing=locked \
    --mount=type=cache,id=aqw-component-raster-target-arm64,target=/build/target,sharing=locked \
    . "/root/.cargo/env" \
    && RUSTFLAGS="-C target-feature=+neon" cargo build --release --locked --no-default-features --bin aqw-component-raster \
    && mkdir -p /out \
    && cp target/release/aqw-component-raster /out/bootstrap

# ---- runtime image ---------------------------------------------------------
# resvg is linked statically into the bootstrap; only the binary ships.
FROM public.ecr.aws/lambda/provided:al2023

COPY --from=build /out/bootstrap ${LAMBDA_RUNTIME_DIR}/bootstrap
RUN chmod +x ${LAMBDA_RUNTIME_DIR}/bootstrap

CMD ["bootstrap"]
