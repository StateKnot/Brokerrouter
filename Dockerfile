# syntax=docker/dockerfile:1.7
FROM rust@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS build

WORKDIR /src
ENV CARGO_BUILD_JOBS=1

COPY Cargo.toml Cargo.lock ./
COPY deploy/production/cargo-config.toml /usr/local/cargo/config.toml
COPY src ./src
COPY migrations ./migrations
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --locked --release --bin brokerrouter && \
    cp /src/target/release/brokerrouter /tmp/brokerrouter

FROM debian@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251

RUN apt-get -o Acquire::ForceIPv4=true -o Acquire::http::Timeout=30 \
      -o Acquire::Retries=2 -o APT::Update::Error-Mode=any update && \
    apt-get -o Acquire::ForceIPv4=true -o Acquire::http::Timeout=30 \
      -o Acquire::Retries=2 install --yes --no-install-recommends ca-certificates curl gosu && \
    rm -rf /var/lib/apt/lists/* && \
    groupadd --gid 10001 brokerrouter && \
    useradd --uid 10001 --gid 10001 --no-create-home --home-dir /nonexistent \
      --shell /usr/sbin/nologin brokerrouter

COPY --from=build /tmp/brokerrouter /usr/local/bin/brokerrouter
COPY deploy/production/container-entrypoint.sh /usr/local/bin/container-entrypoint

ENTRYPOINT ["/usr/local/bin/container-entrypoint"]
CMD ["serve", "--bind", "0.0.0.0:8080"]
