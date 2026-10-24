"""A LiveKit Agents worker for the e2e tests.

The dialer dispatches it by agent name into each call room (explicit
dispatch, `CreateDispatch`). It needs no AI service. In each room it counts
the audio frames of the callee, plays a tone back, and ends when the callee
leaves or the room closes. GET /stats on STATS_PORT reports per room:

  {"rooms": {"<room>": {"agent": "agent-AJ_x", "attempt_id": 5, "loud_in": L,
                        "callee": "sip-attempt-5", "frames_in": N,
                        "frames_out": M, "ended": true}}}

Jobs run as threads, so all rooms share one stats table.
"""

import asyncio
import json
import os
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

import numpy as np
from livekit import rtc
from livekit.agents import AgentServer, AutoSubscribe, JobContext, JobExecutorType, cli

AGENT_NAME = os.environ.get("AGENT_NAME", "e2e-livekit")
STATS_PORT = int(os.environ.get("STATS_PORT", "8088"))
# Leave the call after this many seconds (0: stay until the callee leaves).
MAX_CALL_S = float(os.environ.get("MAX_CALL_S", "0"))
# 16 kHz keeps one worker process light enough for a dozen calls at once.
SAMPLE_RATE, CHANNELS, SAMPLES = 16000, 1, 160  # 10 ms frames
# A frame of the callee whose peak passes this carries a tone, not silence.
LOUD = 2000

_stats: dict = {}
_lock = threading.Lock()


def update(room: str, **fields) -> None:
    with _lock:
        _stats.setdefault(
            room, {"frames_in": 0, "loud_in": 0, "frames_out": 0, "ended": False}
        ).update(fields)


def bump(room: str, key: str) -> None:
    with _lock:
        _stats[room][key] += 1


class StatsHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path != "/stats":
            self.send_response(404)
            self.end_headers()
            return
        with _lock:
            body = json.dumps({"rooms": _stats}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


# The dialer decides how many calls run; the worker must never refuse a job.
server = AgentServer(
    job_executor_type=JobExecutorType.THREAD,
    load_fnc=lambda: 0.0,
    load_threshold=1.0,
)


@server.rtc_session(agent_name=AGENT_NAME)
async def entrypoint(ctx: JobContext) -> None:
    call = json.loads(ctx.job.metadata or "{}")
    room = ctx.room.name
    # The dialer names the callee. A job without it (the callee agent behind
    # LiveKit Cloud SIP, e2e/livekit-cloud) takes the first remote participant.
    callee = call.get("callee_identity")
    update(room, attempt_id=call.get("attempt_id"), callee=callee)
    stop = asyncio.Event()

    def is_callee(participant) -> bool:
        nonlocal callee
        if callee is None:
            callee = participant.identity
            # How LiveKit routed a SIP caller: sip.trunkID, sip.trunkPhoneNumber.
            sip = {k: v for k, v in participant.attributes.items() if k.startswith("sip.")}
            update(room, callee=callee, sip=sip)
        return participant.identity == callee

    async def count(track: rtc.Track) -> None:
        async for event in rtc.AudioStream.from_track(
            track=track, sample_rate=SAMPLE_RATE, num_channels=CHANNELS
        ):
            bump(room, "frames_in")
            pcm = np.frombuffer(event.frame.data, dtype=np.int16)
            if pcm.size and np.abs(pcm).max() > LOUD:
                bump(room, "loud_in")

    @ctx.room.on("track_subscribed")
    def _subscribed(track, _publication, participant):
        if track.kind == rtc.TrackKind.KIND_AUDIO and is_callee(participant):
            asyncio.ensure_future(count(track))

    @ctx.room.on("participant_disconnected")
    def _left(participant):
        if is_callee(participant):
            stop.set()

    ctx.room.on("disconnected", lambda *_: stop.set())

    await ctx.connect(auto_subscribe=AutoSubscribe.AUDIO_ONLY)
    update(room, agent=ctx.room.local_participant.identity)
    source = rtc.AudioSource(SAMPLE_RATE, CHANNELS, queue_size_ms=200)
    track = rtc.LocalAudioTrack.create_audio_track("agent-tone", source)
    await ctx.room.local_participant.publish_track(
        track, rtc.TrackPublishOptions(source=rtc.TrackSource.SOURCE_MICROPHONE)
    )

    n, idx = 0, np.arange(SAMPLES)
    if MAX_CALL_S > 0:
        asyncio.get_running_loop().call_later(MAX_CALL_S, stop.set)
    while not stop.is_set():
        pcm = (0.2 * 32767 * np.sin(2 * np.pi * 440 * (idx + n) / SAMPLE_RATE)).astype(np.int16)
        n += SAMPLES
        await source.capture_frame(
            rtc.AudioFrame(
                data=pcm.tobytes(),
                sample_rate=SAMPLE_RATE,
                num_channels=CHANNELS,
                samples_per_channel=SAMPLES,
            )
        )
        bump(room, "frames_out")
    update(room, ended=True)
    ctx.shutdown(reason="the call ended")


if __name__ == "__main__":
    threading.Thread(
        target=lambda: HTTPServer(("0.0.0.0", STATS_PORT), StatsHandler).serve_forever(),
        daemon=True,
    ).start()
    cli.run_app(server)
