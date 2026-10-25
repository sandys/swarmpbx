# The node image with a wrong OS clock, for assert (h) of run_cluster_e2e.sh.
# offset_clock.so moves the wall clock of the process by OFFSET_S seconds.
# Test only: production never preloads it.
ARG BASE=agentdialer:e2e
FROM debian:bookworm-slim AS build
RUN apt-get update && apt-get install -y --no-install-recommends gcc libc6-dev \
    && rm -rf /var/lib/apt/lists/*
COPY offset_clock.c /src/
RUN gcc -O2 -Wall -Werror -shared -fPIC -o /src/offset_clock.so /src/offset_clock.c -ldl

FROM ${BASE}
COPY --from=build /src/offset_clock.so /usr/local/lib/offset_clock.so
ARG OFFSET_S
ENV LD_PRELOAD=/usr/local/lib/offset_clock.so CLOCK_OFFSET_S=${OFFSET_S}
