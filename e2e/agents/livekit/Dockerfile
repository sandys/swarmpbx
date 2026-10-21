# A LiveKit Agents worker for the e2e tests (no AI services).
FROM python:3.12-slim
RUN pip install --no-cache-dir "livekit-agents==1.8.8"
WORKDIR /app
COPY agent.py .
CMD ["python", "agent.py", "start"]
