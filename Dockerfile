# Runtime image for the agentdialer binary (api / orchestrator / dispatch /
# migrate roles). Used by the e2e harness and later by Helm.
FROM rust:1.90-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
      pkg-config libssl-dev ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# ad_bin pulls ad_gateway with the livekit feature already.
RUN cargo build --release -p ad_bin

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/agentdialer /usr/local/bin/agentdialer
ENTRYPOINT ["agentdialer"]
