#!/usr/bin/env bash
# Smoke test against a running codexs (default http://127.0.0.1:8790).
# Usage: tests/smoke.sh [base_url] [api_key]
set -u
BASE="${1:-http://127.0.0.1:8790}"
KEY="${2:-}"
AUTH=()
if [ -n "$KEY" ]; then AUTH=(-H "Authorization: Bearer $KEY"); fi

echo "== GET /v1/models"
curl -sS "${AUTH[@]}" "$BASE/v1/models" | head -c 600; echo; echo

echo "== POST /v1/chat/completions (non-stream)"
curl -sS "${AUTH[@]}" "$BASE/v1/chat/completions" -H 'content-type: application/json' -d '{
  "model": "",
  "messages": [{"role":"user","content":"Reply with exactly the word OK and nothing else."}]
}' | head -c 1500; echo; echo

echo "== POST /v1/responses (stream, client tool)"
curl -sS -N "${AUTH[@]}" "$BASE/v1/responses" -H 'content-type: application/json' -d '{
  "model": "",
  "stream": true,
  "instructions": "You are a terse assistant.",
  "tools": [{"type":"function","name":"get_weather","description":"Get the current weather for a city.","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}],
  "input": "What is the weather in Singapore right now? Use the tool."
}' | tee /tmp/asxs_resp1.txt | grep -E '^event:' | sort | uniq -c
echo
RESP_ID=$(grep -o '"id":"resp_[a-f0-9]*"' /tmp/asxs_resp1.txt | head -1 | cut -d'"' -f4)
CALL_ID=$(grep -o '"call_id":"[^"]*"' /tmp/asxs_resp1.txt | head -1 | cut -d'"' -f4)
echo "response_id=$RESP_ID call_id=$CALL_ID"

if [ -n "$CALL_ID" ]; then
  echo "== POST /v1/responses (continue with tool output)"
  curl -sS "${AUTH[@]}" "$BASE/v1/responses" -H 'content-type: application/json' -d "{
    \"model\": \"\",
    \"previous_response_id\": \"$RESP_ID\",
    \"tools\": [{\"type\":\"function\",\"name\":\"get_weather\",\"description\":\"Get the current weather for a city.\",\"parameters\":{\"type\":\"object\",\"properties\":{\"city\":{\"type\":\"string\"}},\"required\":[\"city\"]}}],
    \"input\": [{\"type\":\"function_call_output\",\"call_id\":\"$CALL_ID\",\"output\":\"31°C, humid, light rain\"}]
  }" | head -c 2000; echo
fi

echo; echo "== GET /v1/sessions"
curl -sS "${AUTH[@]}" "$BASE/v1/sessions" | head -c 800; echo
