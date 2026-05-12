"""Воспроизвести поведение supervisor: запуск cache-ci через subprocess.Popen
и циклически делать poll() — увидеть когда rc становится не None и какой
именно код возвращается."""
import subprocess
import time
from pathlib import Path

CMD = [
    r"C:\tools\mcp-cache-ci\bin\mcp-cache-ci.exe",
    "--config", r"C:\tools\mcp-cache-ci\config\cache-ci.toml",
]
CWD = r"C:\tools\mcp-cache-ci"
CREATE_NO_WINDOW = 0x08000000

logf = open("C:/Tools/mcp-services-cash/scripts/repro.log", "a", encoding="utf-8", buffering=1)
logf.write(f"\n=== {time.strftime('%H:%M:%S')} START ===\n")
logf.flush()

proc = subprocess.Popen(
    CMD,
    cwd=CWD,
    stdin=subprocess.DEVNULL,
    stdout=logf,
    stderr=logf,
    creationflags=CREATE_NO_WINDOW,
)
print(f"[t=0] PID={proc.pid}")

for sec in range(1, 31):
    time.sleep(1)
    rc = proc.poll()
    print(f"[t={sec}] poll()={rc}")
    if rc is not None:
        print(f"  -> процесс завершился с кодом {rc} через {sec} сек")
        break
else:
    print("[done] процесс жив все 30 секунд")

# Прибиваем
if proc.poll() is None:
    proc.terminate()
    proc.wait(timeout=5)
    print("[cleanup] terminated")
logf.close()
