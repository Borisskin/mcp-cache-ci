"""Симулятор MCP-клиента LibreChat для проверки кэша rag-cache-ci.

Делает:
  1. /metrics — снимок ДО.
  2. MCP handshake (initialize → notifications/initialized) к http://203.0.113.10:8011/mcp/.
  3. tools/call один и тот же запрос ДВА раза подряд:
     первый = miss (или hit если LibreChat уже звонил с такими же args),
     второй = hit (наш ответ уже в кэше).
  4. /metrics — снимок ПОСЛЕ.
  5. Печатает diff.

Нужен только httpx и std stdlib.
"""
from __future__ import annotations

import json
import sys
import time
import uuid

import httpx

# Windows-консоль по умолчанию cp1251 — UTF-8 нужен для русских строк и стрелок.
if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8")
    sys.stderr.reconfigure(encoding="utf-8")

CACHE_URL = "http://203.0.113.10:8011/mcp/"
METRICS_URL = "http://203.0.113.10:8011/metrics"

# Уникальный query чтобы получить детерминированный miss→hit.
# Берём имя функции из УТ + случайный ID, чтобы LibreChat ещё не звонил с такими args.
RUN_ID = uuid.uuid4().hex[:8]
TOOL_NAME = "search_function"
TOOL_ARGS = {
    "repo": "ut",
    "query": f"ОбработкаПроведения {RUN_ID}",
    "limit": 1,
}


def parse_sse_or_json(text: str) -> dict | None:
    """rmcp Streamable-HTTP отвечает либо чистым JSON, либо SSE с несколькими
    data: строками (первая может быть пустая retry-event). Берём первый
    непустой JSON-блок."""
    text = text.strip()
    if not text:
        return None
    if text.startswith("{"):
        return json.loads(text)
    for line in text.splitlines():
        line = line.strip()
        if not line.startswith("data:"):
            continue
        payload = line[len("data:") :].strip()
        if not payload:
            continue
        try:
            return json.loads(payload)
        except json.JSONDecodeError:
            continue
    return None  # SSE без полезного JSON (например, чистый retry-event)


def fetch_metrics(client: httpx.Client) -> dict:
    r = client.get(METRICS_URL, timeout=5.0)
    r.raise_for_status()
    return r.json()


def mcp_call(client: httpx.Client, session_id: str | None, payload: dict) -> tuple[dict | None, str | None]:
    headers = {
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
    }
    if session_id is not None:
        headers["Mcp-Session-Id"] = session_id
    r = client.post(CACHE_URL, headers=headers, json=payload, timeout=15.0)
    r.raise_for_status()
    new_sid = r.headers.get("Mcp-Session-Id") or session_id
    body = r.text.strip()
    if not body:
        return None, new_sid
    return parse_sse_or_json(body), new_sid


def main() -> int:
    with httpx.Client() as client:
        before = fetch_metrics(client)
        print("=== /metrics ДО ===")
        print(json.dumps(before, ensure_ascii=False))

        # 1) initialize
        init_payload = {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "manual-cache-probe", "version": "1.0"},
            },
        }
        init_resp, sid = mcp_call(client, None, init_payload)
        print(f"\n=== initialize → session_id={sid} ===")
        if init_resp:
            srv = init_resp.get("result", {}).get("serverInfo", {})
            print(f"server_info: {srv}")

        # 2) notifications/initialized
        notif_payload = {
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {},
        }
        mcp_call(client, sid, notif_payload)

        # 3) tools/call (1-й раз)
        call_payload = {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": TOOL_NAME, "arguments": TOOL_ARGS},
        }
        t0 = time.perf_counter()
        first, sid = mcp_call(client, sid, call_payload)
        first_ms = (time.perf_counter() - t0) * 1000
        print(f"\n=== tools/call #1 ({first_ms:.1f} ms) ===")
        print("ожидаем miss (новый query)")

        # 4) tools/call (2-й раз — те же args)
        call_payload["id"] = 3
        t0 = time.perf_counter()
        second, sid = mcp_call(client, sid, call_payload)
        second_ms = (time.perf_counter() - t0) * 1000
        print(f"\n=== tools/call #2 ({second_ms:.1f} ms) ===")
        print("ожидаем hit (тот же query)")

        # 5) /metrics ПОСЛЕ
        after = fetch_metrics(client)
        print("\n=== /metrics ПОСЛЕ ===")
        print(json.dumps(after, ensure_ascii=False))

        print("\n=== ДЕЛЬТА ===")
        for k in ("cache_hits", "cache_misses", "bypass", "cache_size", "backend_errors"):
            d = after.get(k, 0) - before.get(k, 0)
            sign = "+" if d > 0 else ""
            print(f"  {k}: {before.get(k, 0)} → {after.get(k, 0)}  ({sign}{d})")

        print(f"\nLatency #1 = {first_ms:.1f} ms (на miss идёт на backend)")
        print(f"Latency #2 = {second_ms:.1f} ms (на hit отдаётся из памяти)")

        return 0


if __name__ == "__main__":
    sys.exit(main())
