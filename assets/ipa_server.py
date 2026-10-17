"""SimpleSTT IPA engine: POWSM phone recognition behind a tiny local HTTP server.

SimpleSTT writes this file next to its private Python environment and starts it on demand.
Protocol: POST /ipa with the raw body = 16 kHz mono signed 16-bit little-endian PCM (up to 20 s)
          -> {"text": "<IPA phones>"}.  The socket only starts listening once the model is loaded.
"""
import argparse
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

import numpy as np

ap = argparse.ArgumentParser()
ap.add_argument("--root", required=True)  # <models folder>/powsm
ap.add_argument("--port", type=int, required=True)
ap.add_argument("--lang", default="<unk>")  # ISO 639-3 in angle brackets, e.g. <eng>; <unk> = any/unknown
args = ap.parse_args()
os.chdir(args.root)  # the model config refers to files relative to the model folder

import torch  # noqa: E402
from espnet2.bin.s2t_inference import Speech2Text  # noqa: E402

EXP = "exp/s2t_train_s2t_ebf_conv2d_size768_e9_d9_piecewise_lr5e-4_warmup60k_flashattn_raw_bpe40000"
device = "cuda" if torch.cuda.is_available() else "cpu"


def load(lang):
    return Speech2Text(
        s2t_train_config=f"{EXP}/config.yaml",
        s2t_model_file=f"{EXP}/valid.acc.ave_5best.till45epoch.pth",
        device=device,
        lang_sym=lang,
        task_sym="<pr>",
    )


try:
    s2t = load(args.lang)
    if args.lang not in s2t.converter.token_list:
        raise KeyError(args.lang)
except Exception:  # language token the model doesn't know -> language-agnostic
    s2t = load("<unk>")

WINDOW = 20 * 16000  # POWSM was trained on fixed 20 s windows


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        x = np.frombuffer(raw[: len(raw) // 2 * 2], dtype="<i2").astype(np.float32) / 32768.0
        x = np.pad(x[:WINDOW], (0, max(0, WINDOW - len(x))))
        try:
            pred = s2t(x, text_prev="<na>")[0][0]
            text = pred.split("<notimestamps>")[-1].replace("/", "").strip()
            body, code = json.dumps({"text": text}, ensure_ascii=False).encode("utf-8"), 200
        except Exception as e:  # keep serving; SimpleSTT shows a generic error
            body, code = json.dumps({"error": str(e)}).encode("utf-8"), 500
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


sys.stdout.flush()
HTTPServer(("127.0.0.1", args.port), Handler).serve_forever()
