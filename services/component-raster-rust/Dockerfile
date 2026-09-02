# Multi-stage build for the aqw-component-raster Lambda.
#
# Builds inside `provided:al2023` (the same glibc 2.34 as the runtime) with
# the conservative AWS x86_64 CPU baseline, so the release bootstrap is a
# lean x86-64 ELF with no musl cross-compilation and no resvg CLI process.

FROM public.ecr.aws/lambda/provided:al2023 AS build

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

# AWS Lambda x86_64 compiler flags plus a thin-LTO, single-CGU, stripped
# release profile for a fast cold start.
ENV RUSTFLAGS="-C target-cpu=x86-64-v2" \
    CARGO_PROFILE_RELEASE_LTO="thin" \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS="1" \
    CARGO_PROFILE_RELEASE_STRIP="symbols" \
    CARGO_PROFILE_RELEASE_PANIC="abort"
RUN . "$HOME/.cargo/env" && cargo build --release --locked --bin aqw-component-raster

# ---- runtime image ---------------------------------------------------------
# resvg is linked statically into the bootstrap; only the binary ships.
FROM public.ecr.aws/lambda/provided:al2023

COPY --from=build /build/target/release/aqw-component-raster ${LAMBDA_RUNTIME_DIR}/bootstrap
RUN chmod +x ${LAMBDA_RUNTIME_DIR}/bootstrap

CMD ["bootstrap"]