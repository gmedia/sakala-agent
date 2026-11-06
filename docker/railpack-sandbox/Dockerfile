# Runs Railpack and mise on repositories Sakala does not trust.
#
# mise renders tool versions from a repository's own config, including Tera
# `exec`, so analysing a repository can run its code. This image is the boundary
# for that, together with the options the Agent gives `docker run`. It holds no
# secrets, and both binaries are baked in so a run downloads no code of its own.
#
# Build it, then pin its digest in SAKALA_RAILPACK_IMAGE. Keep the Railpack version
# in step with SAKALA_RAILPACK_FRONTEND, and the mise version in step with
# core/mise/version.txt of that Railpack release. Both checksums come from the
# upstream release (checksums.txt and SHASUMS256.txt). x86_64 only.
FROM alpine:3.22@sha256:5291449c3df73caf6ed85e649dec1b9e818b39a5d8c871e97afc13e9cd5e8fa8

ARG RAILPACK_VERSION=v0.23.0
ARG RAILPACK_SHA256=e8bffc181c13e68c1c78ec618f1418e60b1602ec548f3ec1054996394ce0f06a
ARG MISE_VERSION=2026.3.17
ARG MISE_SHA256=f416a2d27f69173b22551429d2bc712c40fc3fb639112cf76ae817baae3772e2

RUN apk add --no-cache ca-certificates

WORKDIR /build
RUN wget -q "https://github.com/railwayapp/railpack/releases/download/${RAILPACK_VERSION}/railpack-${RAILPACK_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
    && echo "${RAILPACK_SHA256}  railpack-${RAILPACK_VERSION}-x86_64-unknown-linux-musl.tar.gz" | sha256sum -c - \
    && tar -xzf "railpack-${RAILPACK_VERSION}-x86_64-unknown-linux-musl.tar.gz" railpack \
    && install -m 0755 railpack /usr/local/bin/railpack \
    && wget -q "https://github.com/jdx/mise/releases/download/v${MISE_VERSION}/mise-v${MISE_VERSION}-linux-x64-musl.tar.gz" \
    && echo "${MISE_SHA256}  mise-v${MISE_VERSION}-linux-x64-musl.tar.gz" | sha256sum -c - \
    && tar -xzf "mise-v${MISE_VERSION}-linux-x64-musl.tar.gz" mise/bin/mise \
    && install -m 0755 mise/bin/mise "/usr/local/bin/mise-${MISE_VERSION}"
WORKDIR /
RUN rm -rf /build

COPY entrypoint.sh /usr/local/bin/sandbox-entrypoint
RUN chmod 0755 /usr/local/bin/sandbox-entrypoint

ENV MISE_VERSION=${MISE_VERSION}
USER 65534:65534
ENTRYPOINT ["/usr/local/bin/sandbox-entrypoint"]
