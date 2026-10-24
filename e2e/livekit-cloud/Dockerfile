# The LiveKit Cloud test tool: the official Python SDK, and the official lk
# CLI for the phone-number API (the SDK does not have it).
FROM livekit/livekit-cli:v2.19.1 AS lk
FROM python:3.12-slim
RUN pip install --no-cache-dir "livekit-api==1.2.2"
COPY --from=lk /lk /usr/local/bin/lk
