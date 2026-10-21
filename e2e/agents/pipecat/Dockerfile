# A pipecat bot server for the e2e tests (no AI services).
FROM python:3.12-slim
RUN pip install --no-cache-dir "pipecat-ai[livekit]==1.13.0"
WORKDIR /app
COPY bot.py .
CMD ["python", "bot.py"]
