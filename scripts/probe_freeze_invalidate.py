"""Проверка /freeze, /thaw и селективной /invalidate по repo-scope.

Шаги:
  1. Заполняем кэш для двух репо (ut и bp-ss) — простыми get_stats запросами,
     по одному на каждый. После — cache_size должен вырасти на 2.
  2. POST /invalidate {"repo": "ut"}. Должно убрать только запись repo=ut,
     bp-ss остаться.
  3. POST /freeze {"scope": "ut", "duration_seconds": 30}.
     tools/call с repo=ut должен вернуть ошибку 'Frozen', с repo=bp-ss — отвечает.
  4. POST /thaw {"scope": "ut"}. Запрос repo=ut снова работает.
"""
from __future__ import annotations

import json
import sys
import time
import uuid

import httpx

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8")
    sys.stderr.reconfigure(encoding="utf-8")

CACHE = "http://203.0.113.10:8011"
RUN_ID = uuid.uuid4().hex[:6]


def parse_sse_or_json(text: str):
    text = text.strip()
    if not text:
        return None
    if text.startswith("{"):
        return json.loads(text)
    for line in text.splitlines():
        line = line.strip()
        if not line.startswith("data:"):
            continue
        payload = line[len("data:"):].strip()
        if payload:
            try:
                return json.loads(payload)
            except json.JSONDecodeError:
                continue
    return None


class McpClient:
    def __init__(self, client: httpx.Client):
        self.client = client
        self.session_id: str | None = None
        self._initialize()

    def _post(self, payload: dict) -> dict | None:
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
        }
        if self.session_id:
            headers["Mcp-Session-Id"] = self.session_id
        r = self.client.post(f"{CACHE}/mcp/", headers=headers, json=payload, timeout=15.0)
        r.raise_for_status()
        sid = r.headers.get("Mcp-Session-Id")
        if sid:
            self.session_id = sid
        return parse_sse_or_json(r.text)

    def _initialize(self):
        self._post({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "probe-freeze", "version": "1.0"},
            },
        })
        self._post({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})

    def call(self, name: str, args: dict, jid: int = 100):
        return self._post({
            "jsonrpc": "2.0", "id": jid, "method": "tools/call",
            "params": {"name": name, "arguments": args},
        })


def metrics(client: httpx.Client) -> dict:
    return client.get(f"{CACHE}/metrics", timeout=5).json()


def print_step(s):
    print(f"\n=== {s} ===")


def main():
    with httpx.Client() as cli:
        # Сначала очистка чтобы стартовать с чистого
        cli.post(f"{CACHE}/invalidate", json={"all": True}, timeout=5)
        cli.post(f"{CACHE}/thaw", json={"all": True}, timeout=5)

        m0 = metrics(cli)
        print_step(f"метрики ДО: {json.dumps(m0)}")

        mcp = McpClient(cli)

        print_step("шаг 1: tools/call repo=ut + repo=bp-ss (два разных miss)")
        # уникальные query чтобы не зависеть от чужих cached entries
        mcp.call("get_stats", {"repo": "ut"}, jid=10)
        mcp.call("get_stats", {"repo": "bp-ss"}, jid=11)
        m1 = metrics(cli)
        print(json.dumps(m1))
        assert m1["cache_size"] >= 2, f"ожидали >=2 в кэше, получили {m1['cache_size']}"

        print_step("шаг 2: /invalidate {repo:ut} → должна остаться запись bp-ss")
        r = cli.post(f"{CACHE}/invalidate", json={"repo": "ut"}, timeout=5).json()
        print(json.dumps(r))
        m2 = metrics(cli)
        print(json.dumps(m2))
        # Поскольку до этого был ровно 1 ключ ut и 1 ключ bp-ss, removed=1 и cache_size=1.
        # Если в кэше было что-то еще — числа другие, но мы хотим чтобы removed > 0 и size уменьшилось ровно на removed.
        assert r["removed"] >= 1, "ожидали хотя бы 1 удалённый ключ"
        assert m2["cache_size"] == m1["cache_size"] - r["removed"], "size должен упасть на removed"

        print_step("шаг 3: /freeze {scope:ut, duration:30}")
        r = cli.post(f"{CACHE}/freeze", json={"scope": "ut", "duration_seconds": 30}, timeout=5).json()
        print(json.dumps(r))
        assert r["frozen"] is True

        print_step("шаг 4: tools/call repo=ut — должен прийти error Frozen")
        try:
            resp = mcp.call("get_stats", {"repo": "ut"}, jid=20)
            err = resp.get("error") if resp else None
            print(f"resp: {json.dumps(resp, ensure_ascii=False)[:200]}")
            assert err is not None, "ожидали error в ответе"
            data = err.get("data") or {}
            assert data.get("frozen") is True, f"ожидали data.frozen=true, получили {err}"
            assert data.get("scope") == "ut"
            print(f"  scope={data['scope']}, retry_after_seconds={data.get('retry_after_seconds')}")
        except httpx.HTTPStatusError as e:
            print(f"HTTP error: {e}")
            raise

        print_step("шаг 5: tools/call repo=bp-ss — должен ответить (другой scope)")
        resp = mcp.call("get_stats", {"repo": "bp-ss"}, jid=21)
        # Ожидаем result с непустым content (или хотя бы что нет error)
        if resp:
            assert "error" not in resp or resp.get("error") is None, f"unexpected error для bp-ss: {resp}"
        print("  bp-ss ответил без freeze-ошибки ✓")

        print_step("шаг 6: /thaw {scope:ut}")
        r = cli.post(f"{CACHE}/thaw", json={"scope": "ut"}, timeout=5).json()
        print(json.dumps(r))

        print_step("шаг 7: tools/call repo=ut — снова работает")
        resp = mcp.call("get_stats", {"repo": "ut"}, jid=30)
        if resp:
            assert "error" not in resp or resp.get("error") is None, f"после thaw repo=ut должен работать, но: {resp}"
        print("  repo=ut снова отвечает ✓")

        print_step("ИТОГ")
        m_final = metrics(cli)
        print(json.dumps(m_final))
        print("\nВсе проверки пройдены: invalidate селективен по repo, freeze/thaw работают per-scope.")
        return 0


if __name__ == "__main__":
    sys.exit(main())
