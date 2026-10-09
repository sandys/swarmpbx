# Real LiveKit agent for the MVP wiring test.
FROM python:3.12-slim
WORKDIR /app
RUN pip install --no-cache-dir "livekit==1.1.20" "livekit-api==1.2.1"
COPY agent.py /app/agent.py
EXPOSE 8088
CMD ["python", "-u", "agent.py"]
