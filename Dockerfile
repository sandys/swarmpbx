# Runtime image for the agentdialer binary (api / orchestrator / dispatch /
# migrate roles). Used by the e2e harness and by Helm.
#
# The binary links libwebrtc (the WebRTC bridge), which needs clang 21 or later
# and glib-2.0 at build time.
FROM rust:1.90-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates pkg-config libssl-dev libglib2.0-dev python3 \
      wget gnupg lsb-release software-properties-common \
    && rm -rf /var/lib/apt/lists/*
RUN wget -qO /tmp/llvm.sh https://apt.llvm.org/llvm.sh \
    && chmod +x /tmp/llvm.sh \
    && /tmp/llvm.sh 21 \
    && rm -rf /var/lib/apt/lists/* /tmp/llvm.sh
ENV CC=clang-21 CXX=clang++-21
WORKDIR /src
COPY . .
RUN cargo build --release -p ad_bin

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates libssl3 libglib2.0-0 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/agentdialer /usr/local/bin/agentdialer
ENTRYPOINT ["agentdialer"]
