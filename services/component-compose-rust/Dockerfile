# Multi-stage build for the aqw-component-compose Lambda.

# Build stage uses Amazon Linux 2023 (the same glibc 2.34 as the
# provided:al2023 runtime image), so the release binary is ABI-identical to
# the Lambda runtime and needs no musl cross-compilation. RUSTFLAGS pins the
# conservative AWS x86_64 CPU baseline (x86-64-v2: SSE3/SSSE3/SSE4.1/SSE4.2/
# POPCNT/AVX) used for Lambda's Firecracker x86_64 microVMs; this is the same
# instruction baseline cargo-lambda targets for x86_64.
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
RUN . "$HOME/.cargo/env" && cargo build --release --locked --bin aqw-component-compose

# ---- runtime image ---------------------------------------------------------
FROM public.ecr.aws/lambda/provided:al2023

ARG LIBWEBP_VERSION=1.5.0
ARG LIBWEBP_SHA256=f4bf49f85991f50e86a5404d16f15b72a053bb66768ed5cc0f6d042277cc2bb8

# The pinned cwebp binary only (identical build and flags to the Python
# renderer image so the compositor comparison isolates codec behavior).
# curl (curl-minimal) ships with the al2023 base image; tar and gzip are
# added and removed here like the Python renderer image does.
RUN dnf install -y tar gzip \
    && curl --fail --location --retry 3 \
      "https://storage.googleapis.com/downloads.webmproject.org/releases/webp/libwebp-${LIBWEBP_VERSION}-linux-x86-64.tar.gz" \
      --output /tmp/libwebp.tar.gz \
    && echo "${LIBWEBP_SHA256}  /tmp/libwebp.tar.gz" | sha256sum --check --strict \
    && mkdir -p /opt/libwebp \
    && tar -xzf /tmp/libwebp.tar.gz --strip-components=1 -C /opt/libwebp \
    && rm /tmp/libwebp.tar.gz \
    && dnf remove -y tar gzip \
    && dnf clean all \
    && rm -rf /var/cache/dnf \
    && /opt/libwebp/bin/cwebp -version

COPY --from=build /build/target/release/aqw-component-compose ${LAMBDA_RUNTIME_DIR}/bootstrap
RUN chmod +x ${LAMBDA_RUNTIME_DIR}/bootstrap

ENV CHAR_RENDER_CWEBP=/opt/libwebp/bin/cwebp \
    CHAR_RENDER_TEMP_DIR=/tmp

CMD ["bootstrap"]