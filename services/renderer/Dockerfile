FROM public.ecr.aws/lambda/python:3.13

ARG FFDEC_VERSION=26.2.1
ARG FFDEC_SHA256=0333b56998a55bd83f4e0deb678a811fcdc45607582b4f5dd438309c8c3ad5ce
ARG LIBWEBP_VERSION=1.5.0
ARG LIBWEBP_SHA256=f4bf49f85991f50e86a5404d16f15b72a053bb66768ed5cc0f6d042277cc2bb8

RUN dnf install -y \
      findutils \
      gzip \
      java-21-amazon-corretto-headless \
      librsvg2-tools \
      tar \
      unzip \
    && dnf clean all \
    && rm -rf /var/cache/dnf

RUN curl --fail --location --retry 3 \
      "https://github.com/jindrapetrik/jpexs-decompiler/releases/download/version${FFDEC_VERSION}/ffdec_${FFDEC_VERSION}.zip" \
      --output /tmp/ffdec.zip \
    && echo "${FFDEC_SHA256}  /tmp/ffdec.zip" | sha256sum --check --strict \
    && mkdir -p /opt/ffdec \
    && unzip -q /tmp/ffdec.zip -d /opt/ffdec \
    && rm /tmp/ffdec.zip

RUN curl --fail --location --retry 3 \
      "https://storage.googleapis.com/downloads.webmproject.org/releases/webp/libwebp-${LIBWEBP_VERSION}-linux-x86-64.tar.gz" \
      --output /tmp/libwebp.tar.gz \
    && echo "${LIBWEBP_SHA256}  /tmp/libwebp.tar.gz" | sha256sum --check --strict \
    && mkdir -p /opt/libwebp \
    && tar -xzf /tmp/libwebp.tar.gz --strip-components=1 -C /opt/libwebp \
    && rm /tmp/libwebp.tar.gz \
    && /opt/libwebp/bin/cwebp -version \
    && /opt/libwebp/bin/webpmux -version

COPY pyproject.toml README.md ${LAMBDA_TASK_ROOT}/
COPY src ${LAMBDA_TASK_ROOT}/src

RUN python -m pip install --no-cache-dir ${LAMBDA_TASK_ROOT}

ENV CHAR_RENDER_FFDEC_PATH=/opt/ffdec/ffdec-cli.jar \
    CHAR_RENDER_RSVG_CONVERT=/usr/bin/rsvg-convert \
    CHAR_RENDER_CWEBP=/opt/libwebp/bin/cwebp \
    CHAR_RENDER_WEBPMUX=/opt/libwebp/bin/webpmux \
    JAVA_TOOL_OPTIONS=-Djava.awt.headless=true

CMD ["aqw_char_renderer.handlers.launcher.handler"]
