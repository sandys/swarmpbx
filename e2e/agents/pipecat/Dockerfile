# A pipecat bot server for the e2e tests (no AI services). Build it with the
# additional contexts `proto` (the repository's proto/) and `session`
# (e2e/agents/session), for the client of the session stream.
FROM python:3.12-slim
RUN pip install --no-cache-dir "pipecat-ai[livekit]==1.13.0" \
    "grpcio==1.81.1" "grpcio-tools==1.81.1" "protobuf==6.33.6"
# The default gRPC resolver (c-ares) took 3-6 s for each connect on the
# Docker DNS; the native resolver takes milliseconds.
ENV GRPC_DNS_RESOLVER=native
WORKDIR /app
COPY --from=proto agentdialer/session/v1/session.proto /proto/agentdialer/session/v1/
RUN python -m grpc_tools.protoc -I /proto --python_out=. --grpc_python_out=. \
    agentdialer/session/v1/session.proto
COPY --from=session agent_session.py .
COPY bot.py .
CMD ["python", "bot.py"]
