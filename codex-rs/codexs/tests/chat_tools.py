#!/usr/bin/env python3
"""Stateless Chat Completions round-trip with a client tool (stdlib only).

Usage: python tests/chat_tools.py [base_url] [api_key] [--stream]
"""
import json
import sys
import urllib.request

BASE = sys.argv[1] if len(sys.argv) > 1 and not sys.argv[1].startswith("--") else "http://127.0.0.1:8790"
KEY = sys.argv[2] if len(sys.argv) > 2 and not sys.argv[2].startswith("--") else ""
STREAM = "--stream" in sys.argv

TOOLS = [{
    "type": "function",
    "function": {
        "name": "get_weather",
        "description": "Get the current weather for a city.",
        "parameters": {
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"],
        },
    },
}]


def post(path, body):
    req = urllib.request.Request(
        BASE + path,
        data=json.dumps(body).encode(),
        headers={"content-type": "application/json", "x-asxs-codex-tools": "none", **({"authorization": f"Bearer {KEY}"} if KEY else {})},
    )
    resp = urllib.request.urlopen(req, timeout=600)
    if not STREAM:
        return json.loads(resp.read())
    # Reassemble a streamed completion into a message.
    message = {"role": "assistant", "content": "", "tool_calls": []}
    finish = None
    reasoning = ""
    for raw in resp:
        line = raw.decode().strip()
        if not line.startswith("data:"):
            continue
        data = line[5:].strip()
        if data == "[DONE]":
            break
        chunk = json.loads(data)
        if "error" in chunk:
            raise SystemExit(f"stream error: {chunk['error']}")
        for choice in chunk.get("choices", []):
            delta = choice.get("delta", {})
            if delta.get("content"):
                message["content"] += delta["content"]
                sys.stdout.write(delta["content"])
                sys.stdout.flush()
            if delta.get("reasoning_content"):
                reasoning += delta["reasoning_content"]
            for tc in delta.get("tool_calls", []) or []:
                idx = tc["index"]
                while len(message["tool_calls"]) <= idx:
                    message["tool_calls"].append({"id": "", "type": "function", "function": {"name": "", "arguments": ""}})
                slot = message["tool_calls"][idx]
                slot["id"] = tc.get("id") or slot["id"]
                slot["function"]["name"] = tc["function"].get("name") or slot["function"]["name"]
                slot["function"]["arguments"] += tc["function"].get("arguments") or ""
            if choice.get("finish_reason"):
                finish = choice["finish_reason"]
        if chunk.get("usage"):
            print("\nusage:", chunk["usage"])
    if reasoning:
        print("\n[reasoning]", reasoning[:500])
    if not message["tool_calls"]:
        message.pop("tool_calls")
    print()
    return {"choices": [{"message": message, "finish_reason": finish}]}


messages = [
    {"role": "system", "content": "You are a terse assistant. Always use tools when they apply."},
    {"role": "user", "content": "What's the weather in Singapore right now?"},
]
body = {"model": "gpt-5.5", "reasoning_effort": "low", "messages": messages, "tools": TOOLS, "stream": STREAM, "stream_options": {"include_usage": True}}
first = post("/v1/chat/completions", body)
msg = first["choices"][0]["message"]
print("turn 1 finish:", first["choices"][0]["finish_reason"])
print("turn 1 message:", json.dumps(msg, ensure_ascii=False)[:800])
if not msg.get("tool_calls"):
    raise SystemExit("model did not call the tool")

messages.append(msg)
for tc in msg["tool_calls"]:
    messages.append({"role": "tool", "tool_call_id": tc["id"], "content": "31°C, humid, light rain"})

second = post("/v1/chat/completions", {**body, "messages": messages})
print("turn 2 finish:", second["choices"][0]["finish_reason"])
print("turn 2 message:", json.dumps(second["choices"][0]["message"], ensure_ascii=False)[:800])
if "usage" in second:
    print("usage:", second["usage"])

# Third turn: plain follow-up on the same (stateless) conversation.
messages.append(second["choices"][0]["message"])
messages.append({"role": "user", "content": "Thanks. In one word, should I bring an umbrella?"})
third = post("/v1/chat/completions", {**body, "messages": messages})
print("turn 3 message:", json.dumps(third["choices"][0]["message"], ensure_ascii=False)[:400])
