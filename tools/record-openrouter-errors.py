#!/usr/bin/env python3
"""Record how OpenRouter reports errors: one request per case, saved as fixtures
(crates/omni-llm/tests/fixtures/errors; tests in crates/omni-llm/src/openrouter/recorded.rs).
Costs well under $0.01 for all cases.

tools/record-openrouter-errors.py <out_dir> [case ...]
Each case writes <out_dir>/<case>.json: {"status", "headers" (selected), "body"}
for errors before streaming, or {"status", "sse": [data payloads]} for streams.
Account identifiers are replaced by placeholders. The API key comes from .env
and is never printed or stored.
"""
import base64, json, os, re, sys, time, urllib.error, urllib.request

ROOT = "/home/zzh1996/projects/chatgpt-telegram-bot-omni"
env = dict(l.split("=", 1) for l in open(f"{ROOT}/.env").read().splitlines() if "=" in l and not l.startswith("#"))
KEY = env["OPENROUTER_API_KEY"].strip()
BASE = "https://openrouter.ai/api/v1"
DEBUG = {"echo_upstream_body": True}


def pin(slug):
    return {"only": [slug], "allow_fallbacks": False, "require_parameters": True}


BAD_PNG = "data:image/png;base64," + base64.b64encode(b"\x89PNG\r\n\x1a\nnot really a png").decode()
LONG = "lorem ipsum dolor sit amet " * 30000  # ~150K tokens

CASES = {
    "cc_bad_key": ("chat", {"model": "deepseek/deepseek-v4.1-flash-1017", "stream": True,
                            "messages": [{"role": "user", "content": "hi"}]}, "sk-or-v1-invalid"),
    "cc_unknown_model": ("chat", {"model": "openai/gpt-does-not-exist", "stream": True,
                                  "messages": [{"role": "user", "content": "hi"}]}, None),
    "cc_no_endpoint": ("chat", {"model": "google/gemini-3.5-flash-lite", "stream": True, "provider": pin("anthropic"),
                                "messages": [{"role": "user", "content": "hi"}]}, None),
    "cc_cannot_afford": ("chat", {"model": "openai/gpt-5.5-pro", "stream": True, "provider": pin("openai"),
                                  "max_tokens": 128000, "messages": [{"role": "user", "content": "hi"}]}, None),
    "rsp_cannot_afford": ("responses", {"model": "openai/gpt-5.5-pro", "stream": True, "provider": pin("openai"), "store": False,
                                        "max_output_tokens": 128000,
                                        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]}, None),
    "cc_context_too_long": ("chat", {"model": "z-ai/glm-4.5-air", "stream": True, "provider": pin("z-ai"), "debug": DEBUG,
                                     "messages": [{"role": "user", "content": LONG}]}, None),
    "msg_context_too_long": ("messages", {"model": "anthropic/claude-haiku-4.5", "stream": True, "provider": pin("anthropic"),
                                          "max_tokens": 64, "messages": [{"role": "user", "content": LONG + LONG}]}, None),
    "cc_length": ("chat", {"model": "deepseek/deepseek-v4.1-flash-1017", "stream": True, "provider": pin("deepseek"), "debug": DEBUG,
                           "reasoning": {"effort": "none"}, "max_tokens": 16,
                           "messages": [{"role": "user", "content": "Count from 1 to 100, separated by spaces."}]}, None),
    "rsp_length": ("responses", {"model": "openai/gpt-6-luna", "stream": True, "provider": pin("openai"), "store": False, "debug": DEBUG,
                                 "reasoning": {"effort": "none"}, "max_output_tokens": 16,
                                 "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Count from 1 to 100, separated by spaces."}]}]}, None),
    "rsp_reasoning_ate_budget": ("responses", {"model": "openai/gpt-6-luna", "stream": True, "provider": pin("openai"), "store": False,
                                               "reasoning": {"effort": "high", "summary": "detailed"}, "max_output_tokens": 24,
                                               "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "What is 37 * 43 - 12? Think carefully."}]}]}, None),
    "msg_length": ("messages", {"model": "anthropic/claude-haiku-5.5", "stream": True, "provider": pin("anthropic"),
                                "thinking": {"type": "disabled"}, "max_tokens": 16,
                                "messages": [{"role": "user", "content": "Count from 1 to 100, separated by spaces."}]}, None),
    "cc_reasoning_mandatory_off": ("chat", {"model": "google/gemini-3.8-flash", "stream": True, "provider": pin("google-ai-studio"),
                                            "reasoning": {"effort": "none"}, "messages": [{"role": "user", "content": "hi"}]}, None),
    "cc_bad_image_gemini": ("chat", {"model": "google/gemini-3.5-flash-lite", "stream": True, "provider": pin("google-ai-studio"), "debug": DEBUG,
                                     "messages": [{"role": "user", "content": [{"type": "text", "text": "What is this?"},
                                                                               {"type": "image_url", "image_url": {"url": BAD_PNG}}]}]}, None),
    "msg_bad_image_claude": ("messages", {"model": "anthropic/claude-haiku-5.5", "stream": True, "provider": pin("anthropic"), "max_tokens": 64,
                                          "messages": [{"role": "user", "content": [{"type": "text", "text": "What is this?"},
                                                                                    {"type": "image", "source": {"type": "base64", "media_type": "image/png",
                                                                                                                 "data": BAD_PNG.split(",", 1)[1]}}]}]}, None),
    "rsp_bad_image_openai": ("responses", {"model": "openai/gpt-6-luna", "stream": True, "provider": pin("openai"), "store": False, "debug": DEBUG,
                                           "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "What is this?"},
                                                                                                     {"type": "input_image", "image_url": BAD_PNG, "detail": "low"}]}]}, None),
    "cc_audio_to_text_model": ("chat", {"model": "deepseek/deepseek-v4-pro-0813", "stream": True, "provider": pin("deepseek"),
                                        "messages": [{"role": "user", "content": [{"type": "text", "text": "transcribe"},
                                                                                  {"type": "input_audio", "input_audio": {"data": base64.b64encode(b"RIFF0000WAVE").decode(), "format": "wav"}}]}]}, None),
    "msg_max_tokens_too_big": ("messages", {"model": "anthropic/claude-haiku-5.5", "stream": True, "provider": pin("anthropic"), "max_tokens": 500000,
                                            "messages": [{"role": "user", "content": "hi"}]}, None),
    "msg_empty_message": ("messages", {"model": "anthropic/claude-haiku-5.5", "stream": True, "provider": pin("anthropic"), "max_tokens": 64,
                                       "messages": [{"role": "user", "content": ""}]}, None),
}

SCRUB = [
    (re.compile(r'user_[A-Za-z0-9]{20,}'), "user_REDACTED"),
    (re.compile(r'"safety_identifier"\s*:\s*"[0-9a-f]{32,}"'), '"safety_identifier": "REDACTED"'),
    (re.compile(r"workspaces/[A-Za-z0-9_-]+"), "workspaces/WORKSPACE"),
    (re.compile(r"/keys/[0-9a-f]{32,}"), "/keys/KEYHASH"),
]


def scrub(s: str) -> str:
    for rx, rep in SCRUB:
        s = rx.sub(rep, s)
    return s


def run(name, ep, body, key):
    url = {"chat": f"{BASE}/chat/completions", "responses": f"{BASE}/responses", "messages": f"{BASE}/messages"}[ep]
    req = urllib.request.Request(url, data=json.dumps(body).encode(), method="POST", headers={
        "Authorization": f"Bearer {key or KEY}", "Content-Type": "application/json", "X-OpenRouter-Metadata": "enabled"})
    rec = {"case": name, "endpoint": ep, "model": body.get("model"), "recorded": time.strftime("%Y-%m-%d")}
    try:
        with urllib.request.urlopen(req, timeout=300) as r:
            rec["status"] = r.status
            rec["sse"] = []
            for raw in r:
                line = raw.decode("utf-8", "replace").rstrip("\r\n")
                if line.startswith("data:"):
                    d = line[5:].strip()
                    if d == "[DONE]":
                        break
                    rec["sse"].append(json.loads(d))
    except urllib.error.HTTPError as e:
        rec["status"] = e.code
        rec["headers"] = {k: v for k, v in e.headers.items() if k.lower() in ("retry-after", "content-type", "x-openrouter-error-origin")}
        text = e.read().decode("utf-8", "replace")
        try:
            rec["body"] = json.loads(text)
        except json.JSONDecodeError:
            rec["body"] = text
    return rec


def main():
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    names = sys.argv[2:] or list(CASES)
    for name in names:
        ep, body, key = CASES[name]
        rec = run(name, ep, body, key)
        text = scrub(json.dumps(rec, ensure_ascii=False, indent=1))
        open(f"{out}/{name}.json", "w").write(text + "\n")
        summary = rec.get("body") if "body" in rec else [e.get("type") or (e.get("choices") or [{}])[0].get("finish_reason") for e in rec["sse"]][-4:]
        print(f"{name}: HTTP {rec['status']} {scrub(json.dumps(summary, ensure_ascii=False))[:300]}")


main()
