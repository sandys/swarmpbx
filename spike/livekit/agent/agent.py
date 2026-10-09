#!/usr/bin/env python3
"""A real (minimal) LiveKit agent for the MVP wiring test.

This is a genuine LiveKit RTC participant, not a mock:
  * it connects to the LiveKit room over WebRTC using the `livekit` SDK;
  * it SUBSCRIBES to the caller's audio track and counts received frames
    (proof the caller->agent path is wired);
  * it PUBLISHES an audio track (a tone) back into the room
    (proof the agent->caller path is wired).

It deliberately omits STT/LLM/TTS: those need external API keys and are
non-deterministic, which does not belong in an automated wiring test. The
subject under test is the AGENT WIRING (subscribe + publish), not speech AI.

The agent exposes a tiny HTTP endpoint on :8088 so the test harness can poll
readiness and assertion counters deterministically:
  GET /stats -> {"connected":bool,"subscribed_tracks":int,"frames_in":int,
                 "frames_out":int}
"""

import asyncio
import json
import math
import os
import struct
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

from livekit import rtc

ROOM = os.environ.get("AGENT_ROOM", "spike-room")
LK_URL = os.environ.get("LIVEKIT_URL", "ws://127.0.0.1:7880")
LK_API_KEY = os.environ.get("LIVEKIT_API_KEY", "devkey")
LK_API_SECRET = os.environ.get("LIVEKIT_API_SECRET", "devsecret_devsecret_devsecret_32b")
IDENTITY = os.environ.get("AGENT_IDENTITY", "mvp-agent")

SAMPLE_RATE = 48000
NUM_CHANNELS = 1

# Shared counters the HTTP endpoint reports and the test asserts on.
stats = {"connected": False, "subscribed_tracks": 0, "frames_in": 0, "frames_out": 0}
stats_lock = threading.Lock()


def bump(key, by=1):
    with stats_lock:
        stats[key] += by


def set_stat(key, val):
    with stats_lock:
        stats[key] = val


class StatsHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path != "/stats":
            self.send_response(404)
            self.end_headers()
            return
        with stats_lock:
            body = json.dumps(dict(stats)).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass  # keep the test output clean


def serve_stats():
    HTTPServer(("0.0.0.0", 8088), StatsHandler).serve_forever()


def make_token() -> str:
    from livekit import api

    return (
        api.AccessToken(LK_API_KEY, LK_API_SECRET)
        .with_identity(IDENTITY)
        .with_name("MVP Agent")
        .with_grants(api.VideoGrants(room_join=True, room=ROOM, can_subscribe=True, can_publish=True))
        .to_jwt()
    )


async def consume_track(track: rtc.Track):
    """Read audio frames from a subscribed track and count them."""
    stream = rtc.AudioStream(track)
    async for _event in stream:
        bump("frames_in")


async def publish_tone(room: rtc.Room):
    """Publish a continuous 440 Hz tone so the agent->caller path carries media."""
    source = rtc.AudioSource(SAMPLE_RATE, NUM_CHANNELS)
    track = rtc.LocalAudioTrack.create_audio_track("agent-tone", source)
    await room.local_participant.publish_track(
        track, rtc.TrackPublishOptions(source=rtc.TrackSource.SOURCE_MICROPHONE)
    )

    samples_per_frame = SAMPLE_RATE // 100  # 10 ms frames
    phase = 0.0
    inc = 2 * math.pi * 440.0 / SAMPLE_RATE
    while True:
        frame = rtc.AudioFrame.create(SAMPLE_RATE, NUM_CHANNELS, samples_per_frame)
        data = frame.data  # memoryview of int16
        for i in range(samples_per_frame):
            data[i] = int(0.3 * 32767 * math.sin(phase))
            phase += inc
        await source.capture_frame(frame)
        bump("frames_out")
        await asyncio.sleep(0.01)


async def main():
    threading.Thread(target=serve_stats, daemon=True).start()

    room = rtc.Room()

    @room.on("track_subscribed")
    def on_track_subscribed(track, publication, participant):
        if track.kind == rtc.TrackKind.KIND_AUDIO:
            bump("subscribed_tracks")
            asyncio.create_task(consume_track(track))

    await room.connect(LK_URL, make_token())
    set_stat("connected", True)
    print(f"[agent] connected to room '{ROOM}' as '{IDENTITY}'", flush=True)

    # Publish our outbound tone immediately so a caller hears the agent.
    asyncio.create_task(publish_tone(room))

    # Subscribe to any audio tracks already present (e.g. the SIP caller that
    # may join before or after us).
    for participant in room.remote_participants.values():
        for pub in participant.track_publications.values():
            if pub.track and pub.track.kind == rtc.TrackKind.KIND_AUDIO:
                bump("subscribed_tracks")
                asyncio.create_task(consume_track(pub.track))

    # Run forever; the harness tears the container down.
    while True:
        await asyncio.sleep(1)


if __name__ == "__main__":
    asyncio.run(main())
