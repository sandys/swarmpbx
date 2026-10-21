"""A pipecat bot server for the e2e tests.

POST /start takes the dialer's start request (ad_agent, runtime `pipecat`):

  {"url": "<LiveKit URL>", "room_name": "<room>", "token": "<JWT>",
   "identity": "agent-attempt-5", "call": {"attempt_id": 5,
   "callee_identity": "sip-attempt-5", ...}}

and starts one bot that joins the room with pipecat's LiveKit transport. The
bot needs no AI service: it counts the audio frames of the callee, plays a
tone back, and ends when the callee leaves or the room closes. GET /stats
reports per room, in the same shape as the e2e LiveKit agent.
"""

import asyncio
import os

import numpy as np
from aiohttp import web
from pipecat.frames.frames import Frame, InputAudioRawFrame, OutputAudioRawFrame
from pipecat.pipeline.pipeline import Pipeline
from pipecat.pipeline.worker import PipelineParams, PipelineWorker
from pipecat.processors.frame_processor import FrameDirection, FrameProcessor
from pipecat.transports.livekit.transport import LiveKitParams, LiveKitTransport
from pipecat.workers.runner import WorkerRunner

PORT = int(os.environ.get("PORT", "8089"))
TONE_RATE = 16000

stats: dict = {}
tasks: set = set()


def tone(seconds: float = 1.0) -> OutputAudioRawFrame:
    t = np.arange(int(TONE_RATE * seconds)) / TONE_RATE
    pcm = (0.2 * 32767 * np.sin(2 * np.pi * 440 * t)).astype("<i2").tobytes()
    return OutputAudioRawFrame(audio=pcm, sample_rate=TONE_RATE, num_channels=1)

# A frame of the callee whose peak passes this carries a tone, not silence.
LOUD = 2000


class CalleeAudio(FrameProcessor):
    """Count the callee's input audio; pass every other frame on."""

    def __init__(self, room: str, callee: str, **kwargs):
        super().__init__(**kwargs)
        self._room, self._callee = room, callee

    async def process_frame(self, frame: Frame, direction: FrameDirection):
        await super().process_frame(frame, direction)
        if isinstance(frame, InputAudioRawFrame):
            if getattr(frame, "user_id", "") == self._callee:
                stats[self._room]["frames_in"] += 1
                if np.abs(np.frombuffer(frame.audio, dtype="<i2")).max(initial=0) > LOUD:
                    stats[self._room]["loud_in"] += 1
            return
        await self.push_frame(frame, direction)


async def run_bot(req: dict) -> None:
    room, call = req["room_name"], req.get("call", {})
    callee = call.get("callee_identity", "")
    stats[room] = {
        "agent": req["identity"],
        "attempt_id": call.get("attempt_id"),
        "callee": callee,
        "frames_in": 0,
        "loud_in": 0,
        "frames_out": 0,
        "ended": False,
    }
    transport = LiveKitTransport(
        url=req["url"],
        token=req["token"],
        room_name=room,
        params=LiveKitParams(
            audio_in_enabled=True,
            audio_out_enabled=True,
            audio_in_sample_rate=TONE_RATE,
            audio_out_sample_rate=TONE_RATE,
        ),
    )
    worker = PipelineWorker(
        Pipeline([transport.input(), CalleeAudio(room, callee), transport.output()]),
        params=PipelineParams(),
        # Without AI services there are no speaking frames, so the idle
        # timeout would end the bot after 5 minutes.
        idle_timeout_secs=None,
        enable_rtvi=False,
    )
    runner = WorkerRunner(handle_sigint=False)
    await runner.add_workers(worker)

    @transport.event_handler("on_participant_connected")
    async def _joined(_transport, participant_id):
        if participant_id == callee:
            await worker.queue_frames([tone() for _ in range(30)])
            stats[room]["frames_out"] += 30

    @transport.event_handler("on_participant_disconnected")
    async def _left(_transport, participant_id):
        if participant_id == callee:
            await worker.cancel()

    @transport.event_handler("on_disconnected")
    async def _gone(_transport):
        await worker.cancel()

    try:
        await runner.run()
    finally:
        stats[room]["ended"] = True


async def start(request: web.Request) -> web.Response:
    req = await request.json()
    for field in ("url", "room_name", "token", "identity"):
        if not req.get(field):
            return web.json_response({"error": f"{field} is missing"}, status=400)
    task = asyncio.create_task(run_bot(req))
    tasks.add(task)
    task.add_done_callback(tasks.discard)
    return web.json_response({"started": True})


async def get_stats(_request: web.Request) -> web.Response:
    return web.json_response({"rooms": stats})


app = web.Application()
app.add_routes([web.post("/start", start), web.get("/stats", get_stats)])

if __name__ == "__main__":
    web.run_app(app, host="0.0.0.0", port=PORT)
