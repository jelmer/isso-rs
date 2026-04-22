# Isso production Dockerfile.
#
# Three-stage build:
#  1. Node — build the JS bundles consumed by the admin UI / embed script.
#  2. Rust — build the isso binary.
#  3. Alpine — assemble the runtime image with the compiled binary + static
#     assets + templates.

# =======================================================
# Stage 1: Build the Javascript client bundles
# =======================================================

FROM docker.io/node:current-alpine AS isso-js
WORKDIR /src/

# make is not installed by default on alpine
RUN apk add --no-cache make

# Only copy necessities so the npm install cache stays warm across source
# changes.
COPY ["Makefile", "package.json", "package-lock.json", "./"]

# Disable nagware and skip security "audits".
RUN echo -e "audit=false\nfund=false" > /root/.npmrc

RUN make init

COPY ["static/js/", "./static/js/"]

RUN make js


# =======================================================
# Stage 2: Build the isso binary
# =======================================================

FROM docker.io/rust:1-alpine AS isso-builder
WORKDIR /src

# musl toolchain + deps for ring/rustls and sqlite bundled build.
RUN apk add --no-cache musl-dev perl make

COPY ["Cargo.toml", "Cargo.lock", "./"]
COPY ["src/", "./src/"]
COPY ["templates/", "./templates/"]
COPY ["isso.cfg", "./isso.cfg"]

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release \
 && cp target/release/isso /usr/local/bin/isso


# =======================================================
# Stage 3: Runtime image
# =======================================================

FROM docker.io/alpine:3 AS isso
WORKDIR /isso/

COPY --from=isso-builder /usr/local/bin/isso /usr/local/bin/isso
COPY --from=isso-builder /src/templates/ /isso/templates/
COPY --from=isso-builder /src/isso.cfg /isso/isso.cfg
# CSS / images / demo come straight from the build context; the JS bundles
# built in stage 1 land alongside them in /isso/static/js/. Stage 1 only
# produces JS, so copying the whole `static/` from it would drop css/img/demo.
COPY ["static/css/", "/isso/static/css/"]
COPY ["static/img/", "/isso/static/img/"]
COPY ["static/demo/", "/isso/static/demo/"]
COPY --from=isso-js /src/static/js/ /isso/static/js/

LABEL org.opencontainers.image.source=https://github.com/jelmer/isso-rs
LABEL org.opencontainers.image.description="Isso – a lightweight, self-hosted commenting service"
LABEL org.opencontainers.image.licenses=MIT

RUN mkdir /db /config && chmod 1777 /db /config

VOLUME /db /config
EXPOSE 8080

# Default config path, matching the Python image's convention. Override
# with `-e ISSO_SETTINGS=/path/to/other.cfg` (semicolon-separated for
# multi-site). `isso` reads ISSO_SETTINGS as a fallback when no `-c` is
# passed on the command line.
ENV ISSO_SETTINGS=/config/isso.cfg

# Force binding to all interfaces — a config that says
# `listen = http://localhost:8080` (including the default config) would
# otherwise bind the container's loopback, which `docker run -p` can't
# reach. Operators who mount a config with a different listen can
# override with `-e ISSO_LISTEN=http://0.0.0.0:9000` or unset it entirely
# (`--env ISSO_LISTEN=`).
ENV ISSO_LISTEN=http://0.0.0.0:8080

ENTRYPOINT ["/usr/local/bin/isso"]
