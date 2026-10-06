#!/usr/bin/env python3
"""Minimal XTTS v2 server for OxideClaw.

Keeps the model loaded in GPU VRAM for fast synthesis.
Supports both voice cloning (speaker_wav) and built-in speakers.

Usage:  python3 xtts-server.py <port> [--cpu]
API:    POST http://127.0.0.1:<port>/tts  {text, speaker_wav?, speaker?, language?}
Health: GET  http://127.0.0.1:<port>/health
"""

import sys
import json
import io
import wave
import numpy as np
from http.server import HTTPServer, BaseHTTPRequestHandler
from TTS.api import TTS

USE_GPU = "--cpu" not in sys.argv
PORT = int(sys.argv[1]) if len(sys.argv) > 1 and sys.argv[1].isdigit() else 5002

print(f"Loading XTTS v2 model (gpu={USE_GPU})...", flush=True)
model = TTS("tts_models/multilingual/multi-dataset/xtts_v2", gpu=USE_GPU)
print(f"Model loaded. Listening on 127.0.0.1:{PORT}", flush=True)


def wav_bytes(samples, sample_rate=22050):
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
        if self.path == "/health":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({"status": "ok", "gpu": USE_GPU}).encode())
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

            if speaker_wav:
                samples = model.tts(text=text, speaker_wav=speaker_wav, language=language)
            else:
                samples = model.tts(text=text, speaker=speaker, language=language)

            audio = wav_bytes(samples)

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
    server = HTTPServer(("127.0.0.1", PORT), Handler)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    server.server_close()
