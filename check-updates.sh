#!/usr/bin/env bash
# Manual, zero-telemetry update checker against https://bot-tradingsoftware.com/versions.json
set -euo pipefail
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOCAL_VER="$(python3 -c "import json; print(json.load(open('$REPO_DIR/VERSION.json'))['version'])")"
echo "▶ Checking latest version for sentinel-trend (installed: v$LOCAL_VER)..."
python3 - "$LOCAL_VER" <<'PY'
import json, sys, urllib.request
local_v = sys.argv[1]
with urllib.request.urlopen("https://bot-tradingsoftware.com/versions.json", timeout=10) as r:
    data = json.loads(r.read().decode("utf-8"))
app = data.get("apps", {}).get("sentinel-trend", {})
remote_v = app.get("version", data.get("suite_version", local_v))
dl_url = app.get("download_url", "https://bot-tradingsoftware.com/#downloads")
sha = app.get("sha256", "")
if remote_v != local_v:
    print(f"⬆ NEW VERSION AVAILABLE: v{remote_v} (local: v{local_v})")
    print(f"  Download ($0 MIT): {dl_url}")
    if sha:
        print(f"  SHA-256:            {sha}")
else:
    print(f"✔ Up to date (v{local_v}).")
PY
