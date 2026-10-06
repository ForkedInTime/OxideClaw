#!/usr/bin/env python3
"""Minimal XTTS v2 server for OxideClaw.

Keeps the model loaded in GPU VRAM for fast synthesis.
Supports both voice cloning (speaker_wav) and built-in speakers.

Usage:  python3 xtts-server.py <port> [--cpu]
API:    POST http://127.0.0.1:<port>/tts  {text, speaker_wav?, speaker?, language?}
Health: GET  http://127.0.0.1:<port>/health?nonce=<hex>

/health answers with proof = sha256("<token>:<nonce>"), where the token comes
from OXIDECLAW_XTTS_TOKEN. Anything else listening on the port (Coqui's own
tts-server defaults to 5002, or another user's process) cannot produce it, so
OxideClaw never sends it replies to speak.
"""

import sys
import os
import json
import io
import wave
import hashlib
import threading
import numpy as np
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
from urllib.parse import urlsplit, parse_qs
from TTS.api import TTS

TOKEN = os.environ.get("OXIDECLAW_XTTS_TOKEN", "")

USE_GPU = "--cpu" not in sys.argv
PORT = int(sys.argv[1]) if len(sys.argv) > 1 and sys.argv[1].isdigit() else 5002

print(f"Loading XTTS v2 model (gpu={USE_GPU})...", flush=True)
model = TTS("tts_models/multilingual/multi-dataset/xtts_v2", gpu=USE_GPU)
# model.tts() returns raw samples at the model's own rate (24 kHz for XTTS v2);
# a header with any other rate makes players speed up or slow down the voice.
SAMPLE_RATE = getattr(getattr(model, "synthesizer", None), "output_sample_rate", None) or 24000
# Requests are threaded so /health answers while a reply is synthesising;
# the model itself is not thread-safe.
MODEL_LOCK = threading.Lock()
print(f"Model loaded. Listening on 127.0.0.1:{PORT}", flush=True)


def wav_bytes(samples, sample_rate):
    """Convert float32 samples to WAV bytes."""
    buf = io.BytesIO()
    pcm = (np.array(samples) * 32767).clip(-32768, 32767).astype(np.int16)
    with wave.open(buf, "wb") as wf:
        wf.setnchannels(1)
        wf.setsampwidth(2)
        wf.setframerate(sample_rate)
        wf.writeframes(pcm.tobytes())
    return buf.getvalue()


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        url = urlsplit(self.path)
        if url.path == "/health":
            nonce = parse_qs(url.query).get("nonce", [""])[0][:128]
            proof = hashlib.sha256(f"{TOKEN}:{nonce}".encode()).hexdigest() if TOKEN and nonce else ""
            body = {"status": "ok", "service": "oxideclaw-xtts", "gpu": USE_GPU, "proof": proof}
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps(body).encode())
        else:
            self.send_error(404)

    def do_POST(self):
        if self.path != "/tts":
            self.send_error(404)
            return
        try:
            length = int(self.headers.get("Content-Length", 0))
            data = json.loads(self.rfile.read(length)) if length else {}

            text = data.get("text", "")
            if not text:
                self.send_error(400, "missing 'text' field")
                return

            speaker_wav = data.get("speaker_wav")
            speaker = data.get("speaker", "Craig Gutsy")
            language = data.get("language", "en")

            with MODEL_LOCK:
                if speaker_wav:
                    samples = model.tts(text=text, speaker_wav=speaker_wav, language=language)
                else:
                    samples = model.tts(text=text, speaker=speaker, language=language)

            audio = wav_bytes(samples, SAMPLE_RATE)

            self.send_response(200)
            self.send_header("Content-Type", "audio/wav")
            self.send_header("Content-Length", str(len(audio)))
            self.end_headers()
            self.wfile.write(audio)

        except Exception as e:
            self.send_error(500, str(e))

    def log_message(self, format, *args):
        pass  # suppress request logging


if __name__ == "__main__":
    server = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    server.daemon_threads = True
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    server.server_close()
