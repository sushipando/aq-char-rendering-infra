# syntax=docker/dockerfile:1

FROM public.ecr.aws/lambda/provided:al2023 AS build
ARG TARGETARCH
RUN test "${TARGETARCH}" = "arm64" || (echo "pipeline-rust must be built for linux/arm64" >&2; exit 1)
RUN dnf install -y clang cmake gcc gcc-c++ git make perl tar gzip unzip && dnf clean all
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.98.0
ENV PATH="/root/.cargo/bin:${PATH}"
WORKDIR /build
COPY component-raster-rust component-raster-rust
COPY component-compose-rust component-compose-rust
COPY pipeline-rust pipeline-rust
RUN --mount=type=cache,id=aqw-pipeline-cargo-registry-arm64,target=/root/.cargo/registry,sharing=locked \
    --mount=type=cache,id=aqw-pipeline-cargo-git-arm64,target=/root/.cargo/git,sharing=locked \
    --mount=type=cache,id=aqw-pipeline-target-arm64,target=/build/pipeline-rust/target,sharing=locked \
    cd pipeline-rust \
    && RUSTFLAGS="-C target-feature=+neon" cargo build --release --locked \
    && mkdir -p /out \
    && cp target/release/aqw-render-pipeline /out/bootstrap

# Pinned official aarch64 libwebp utilities, matching component-compose.
ARG LIBWEBP_VERSION=1.5.0
RUN curl -fsSL "https://storage.googleapis.com/downloads.webmproject.org/releases/webp/libwebp-${LIBWEBP_VERSION}-linux-aarch64.tar.gz" -o /tmp/webp.tar.gz \
    && echo "ec874dd2e52097f11dd4999176ae6b6fb0ecb165bc1e05d3ecc9b29014fb003e  /tmp/webp.tar.gz" | sha256sum -c - \
    && mkdir -p /opt/libwebp && tar -xzf /tmp/webp.tar.gz --strip-components=1 -C /opt/libwebp

# Original-RGBA sequence encoder, pinned independently from the WebP path.
# libavif's LOCAL AOM dependency is pinned to v3.14.1 in this source release.
RUN curl -fsSL https://codeload.github.com/AOMediaCodec/libavif/tar.gz/refs/tags/v1.4.2 -o /tmp/libavif.tar.gz \
    && echo "2b645287340ba5a631d268b551dc2d72bd73ac33335962dd36dcdb6d8366921d  /tmp/libavif.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/libavif.tar.gz -C /tmp \
    && cmake -S /build/pipeline-rust/native -B /tmp/avif-build \
        -DCMAKE_BUILD_TYPE=Release -DLIBAVIF_SOURCE=/tmp/libavif-1.4.2 \
    && cmake --build /tmp/avif-build -j 2 --target avif-rgba \
    && cp /tmp/avif-build/avif-rgba /out/avif-rgba

FROM public.ecr.aws/lambda/provided:al2023 AS runtime
COPY --from=build /out/bootstrap /var/runtime/bootstrap
COPY --from=build /out/avif-rgba /opt/avif/avif-rgba
ENV CHAR_RENDER_AVIF_RGBA=/opt/avif/avif-rgba
COPY --from=build /opt/libwebp/bin/webpmux /opt/libwebp/bin/webpmux
ENV CHAR_RENDER_WEBPMUX=/opt/libwebp/bin/webpmux
CMD ["bootstrap"]

# Only the source-export function carries a JVM. Bounds/control functions
# select the runtime target above and never launch FFDec.
FROM runtime AS exporter
ARG FFDEC_VERSION=26.2.1
RUN dnf install -y java-21-amazon-corretto-headless unzip && dnf clean all \
    && curl -fsSL "https://github.com/jindrapetrik/jpexs-decompiler/releases/download/version${FFDEC_VERSION}/ffdec_${FFDEC_VERSION}.zip" -o /tmp/ffdec.zip \
    && echo "0333b56998a55bd83f4e0deb678a811fcdc45607582b4f5dd438309c8c3ad5ce  /tmp/ffdec.zip" | sha256sum -c - \
    && mkdir -p /opt/ffdec && unzip -q /tmp/ffdec.zip -d /opt/ffdec
ENV JAVA_TOOL_OPTIONS=-Djava.awt.headless=true
ENV CHAR_RENDER_FFDEC_PATH=/opt/ffdec/ffdec-cli.jar
