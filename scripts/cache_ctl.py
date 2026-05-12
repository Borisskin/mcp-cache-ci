"""cache_ctl — CLI для ручного управления mcp-cache-ci.

Команды
-------
  freeze     — включить block-режим для scope (или global) на N сек
  thaw       — снять заморозку
  invalidate — сбросить кэш (всё / по repo / по file_paths)
  status     — показать /status: freeze + метрики
  health     — короткий /health

Цели (--target / --url) — пресеты:
  remote-ci → http://203.0.113.10:8011  (пример удалённой cache-ci)
  local-ci  → http://127.0.0.1:8011     (cache-ci на этой машине)

Можно передать произвольный URL через --url (тогда --target игнорируется).
Для нескольких целей одной командой — `--target all`.

Примеры
-------
  cache_ctl freeze --target local-ci --repo ut --duration 600
  cache_ctl invalidate --target local-ci --repo ut
  cache_ctl status --target local-ci
"""
from __future__ import annotations

import argparse
import json
import sys
from typing import Any

import httpx

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8")
    sys.stderr.reconfigure(encoding="utf-8")

PRESETS = {
    "remote-ci": "http://203.0.113.10:8011",
    "local-ci":  "http://127.0.0.1:8011",
}
ALL_DEFAULT = ["local-ci", "remote-ci"]


def resolve_targets(target: str | None, url: str | None) -> list[tuple[str, str]]:
    """Возвращает список (label, base_url)."""
    if url:
        return [(url, url.rstrip("/"))]
    if target == "all":
        return [(t, PRESETS[t]) for t in ALL_DEFAULT]
    if target in PRESETS:
        return [(target, PRESETS[target])]
    print(f"ERROR: неизвестный target {target!r}. Доступно: {list(PRESETS)}, all", file=sys.stderr)
    sys.exit(2)


def post(client: httpx.Client, base: str, path: str, body: dict[str, Any]) -> dict | None:
    r = client.post(f"{base}{path}", json=body, timeout=10.0)
    try:
        r.raise_for_status()
    except httpx.HTTPStatusError as e:
        print(f"  HTTP {r.status_code}: {r.text}", file=sys.stderr)
        raise
    return r.json() if r.content else None


def get(client: httpx.Client, base: str, path: str) -> dict:
    r = client.get(f"{base}{path}", timeout=10.0)
    r.raise_for_status()
    return r.json()


def cmd_freeze(args, client: httpx.Client) -> int:
    body = {"duration_seconds": args.duration}
    if args.scope is not None:
        body["scope"] = args.scope
    if args.repo:
        body["repo"] = args.repo
    rc = 0
    for label, url in resolve_targets(args.target, args.url):
        print(f"--- freeze {label} ({url}) ---")
        try:
            print(json.dumps(post(client, url, "/freeze", body), ensure_ascii=False, indent=2))
        except Exception as e:
            print(f"  ошибка: {e}", file=sys.stderr)
            rc = 1
    return rc


def cmd_thaw(args, client: httpx.Client) -> int:
    body: dict[str, Any] = {}
    if args.all:
        body["all"] = True
    if args.scope is not None:
        body["scope"] = args.scope
    if args.repo:
        body["repo"] = args.repo
    rc = 0
    for label, url in resolve_targets(args.target, args.url):
        print(f"--- thaw {label} ({url}) ---")
        try:
            print(json.dumps(post(client, url, "/thaw", body), ensure_ascii=False, indent=2))
        except Exception as e:
            print(f"  ошибка: {e}", file=sys.stderr)
            rc = 1
    return rc


def cmd_invalidate(args, client: httpx.Client) -> int:
    body: dict[str, Any] = {}
    if args.all:
        body["all"] = True
    if args.repo:
        body["repo"] = args.repo
    if args.key_prefix:
        body["key_prefix"] = args.key_prefix
    if not body:
        print("ERROR: укажите хотя бы один из --all/--repo/--key-prefix", file=sys.stderr)
        return 2
    rc = 0
    for label, url in resolve_targets(args.target, args.url):
        print(f"--- invalidate {label} ({url}) ---")
        try:
            print(json.dumps(post(client, url, "/invalidate", body), ensure_ascii=False, indent=2))
        except Exception as e:
            print(f"  ошибка: {e}", file=sys.stderr)
            rc = 1
    return rc


def cmd_status(args, client: httpx.Client) -> int:
    rc = 0
    for label, url in resolve_targets(args.target, args.url):
        print(f"--- status {label} ({url}) ---")
        try:
            print(json.dumps(get(client, url, "/status"), ensure_ascii=False, indent=2))
        except Exception as e:
            print(f"  ошибка: {e}", file=sys.stderr)
            rc = 1
    return rc


def cmd_health(args, client: httpx.Client) -> int:
    rc = 0
    for label, url in resolve_targets(args.target, args.url):
        print(f"--- health {label} ({url}) ---")
        try:
            print(json.dumps(get(client, url, "/health"), ensure_ascii=False, indent=2))
        except Exception as e:
            print(f"  ошибка: {e}", file=sys.stderr)
            rc = 1
    return rc


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--target", choices=list(PRESETS) + ["all"], default="local-ci",
                   help="пресет адреса (local-ci | remote-ci | all). Default: local-ci")
    p.add_argument("--url", help="произвольный URL базы (например http://192.0.2.10:8011) — перебивает --target")
    sub = p.add_subparsers(dest="cmd", required=True)

    f = sub.add_parser("freeze", help="включить block-режим")
    f.add_argument("--scope", help="repo (явно). Пусто/опущено = global")
    f.add_argument("--repo", help="алиас репо для cache-ci")
    f.add_argument("--duration", type=int, default=1800, help="TTL заморозки в секундах (default 1800)")
    f.set_defaults(handler=cmd_freeze)

    t = sub.add_parser("thaw", help="снять заморозку")
    t.add_argument("--scope")
    t.add_argument("--repo")
    t.add_argument("--all", action="store_true", help="снять все заморозки")
    t.set_defaults(handler=cmd_thaw)

    inv = sub.add_parser("invalidate", help="сбросить кэш")
    inv.add_argument("--repo")
    inv.add_argument("--key-prefix")
    inv.add_argument("--all", action="store_true")
    inv.set_defaults(handler=cmd_invalidate)

    st = sub.add_parser("status", help="показать /status")
    st.set_defaults(handler=cmd_status)

    h = sub.add_parser("health", help="показать /health")
    h.set_defaults(handler=cmd_health)

    return p


def main() -> int:
    args = build_parser().parse_args()
    with httpx.Client() as client:
        return args.handler(args, client)


if __name__ == "__main__":
    sys.exit(main())
