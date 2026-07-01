# Multi-stage build of an HTTP-enabled siphon binary.
#
# siphon-http is an addon: the runnable artifact is a `siphon` binary built with
# the `http` feature (from siphon-sip's `siphon-bin` package, which composes the
# addon). This image builds that binary and ships it with a config + script
# mounted at runtime.
#
#   docker build -f deploy/Dockerfile -t siphon-http .
#   docker run -v $PWD/siphon.yaml:/etc/siphon/siphon.yaml \
#              -v $PWD/http.yaml:/etc/siphon/http.yaml \
#              -v $PWD/script.py:/etc/siphon/script.py \
#              -p 8443:8443 siphon-http
#
# NOTE: needs the `http` feature wired into siphon-bin (siphon-sip). Until then,
# build against a branch that carries it.

FROM rust:1-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
        python3-dev libssl-dev pkg-config && rm -rf /var/lib/apt/lists/*
WORKDIR /src
# Build the siphon-bin package with the http feature. Point the build context or
# a git checkout at siphon-sip's siphon-bin; this stage assumes it's vendored at
# ./siphon-bin (adjust to your CI).
COPY . .
RUN cargo build -p siphon-bin --release --features http

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        python3 libpython3.13 ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/siphon /usr/local/bin/siphon
EXPOSE 8443 9090
ENTRYPOINT ["/usr/local/bin/siphon", "-c", "/etc/siphon/siphon.yaml"]
