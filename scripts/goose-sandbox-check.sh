#!/usr/bin/env bash
#
# Isolated goose sandbox health check for honk.
#
# Exits non-zero unless:
# - POST initialize to http://127.0.0.1:${port}/acp with secret returns HTTP 200 + Acp-Connection-Id (then DELETEs it)
# - the goose sessions DB exists under $SANDBOX/data
# - honk-goose-sandbox MainPID has cwd == $SANDBOX and no open fd under ~/.local/share/goose or ~/.config/goose
# - prints "sandbox OK" on success.

set -euo pipefail

SANDBOX=$(realpath -m "${HONK_SANDBOX:-$HOME/.local/state/honk-sandbox}")
case "$SANDBOX" in
    /*) ;;
    *)
        echo "ERROR: SANDBOX path must be absolute: $SANDBOX" >&2
        exit 1
        ;;
esac

PORT="${HONK_SANDBOX_PORT:-4180}"
if [[ "$PORT" == "4080" ]]; then
    echo "ERROR: Refusing to interact with port 4080 (production goose-serve)" >&2
    exit 1
fi

SECRET_FILE="$SANDBOX/secret"
if [[ ! -f "$SECRET_FILE" ]]; then
    echo "ERROR: Secret file $SECRET_FILE not found" >&2
    exit 1
fi
SECRET=$(cat "$SECRET_FILE")

# 1. Verify systemd unit is running
pid=$(systemctl --user show -p MainPID --value honk-goose-sandbox 2>/dev/null || true)
if [[ -z "$pid" || "$pid" == "0" ]]; then
    echo "ERROR: honk-goose-sandbox unit is not running (MainPID: ${pid:-empty})" >&2
    exit 1
fi

if [[ ! -d "/proc/$pid" ]]; then
    echo "ERROR: Process /proc/$pid does not exist" >&2
    exit 1
fi

# 2. Check process cwd is $SANDBOX
actual_cwd=$(readlink "/proc/$pid/cwd" 2>/dev/null || true)
if [[ "$actual_cwd" != "$SANDBOX" ]]; then
    echo "ERROR: Process cwd ($actual_cwd) does not match sandbox ($SANDBOX)" >&2
    exit 1
fi

# 3. POST initialize with bounded retry (up to 15s in case service just started)
tmp_headers=$(mktemp)
tmp_body=$(mktemp)
trap 'rm -f "$tmp_headers" "$tmp_body"' EXIT

http_status=""
for _ in $(seq 1 30); do
    http_status=$(curl -s -S -o "$tmp_body" -D "$tmp_headers" -w "%{http_code}" \
        --connect-timeout 2 \
        -X POST "http://127.0.0.1:${PORT}/acp" \
        -H "Content-Type: application/json" \
        -H "X-Secret-Key: $SECRET" \
        -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}' 2>/dev/null || true)
    if [[ "$http_status" == "200" ]]; then
        break
    fi
    sleep 0.5
done

if [[ "$http_status" != "200" ]]; then
    echo "ERROR: initialize returned HTTP status '$http_status' (expected 200)" >&2
    exit 1
fi

conn_id=$(grep -i '^acp-connection-id:' "$tmp_headers" | awk '{print $2}' | tr -d '\r\n')
if [[ -z "$conn_id" ]]; then
    echo "ERROR: Acp-Connection-Id response header missing" >&2
    exit 1
fi

# DELETE connection ID to avoid leaking it
curl -s -S -f -X DELETE "http://127.0.0.1:${PORT}/acp" \
    -H "Acp-Connection-Id: $conn_id" \
    -H "X-Secret-Key: $SECRET" >/dev/null || {
    echo "ERROR: Failed to DELETE Acp-Connection-Id $conn_id" >&2
    exit 1
}

# 4. Check sessions DB exists under $SANDBOX/data
db_path="$SANDBOX/data/sessions/sessions.db"
if [[ ! -f "$db_path" ]]; then
    echo "ERROR: sessions DB not found at $db_path" >&2
    exit 1
fi

# 5. Check that process has no open fd under ~/.local/share/goose or ~/.config/goose
for fd in "/proc/$pid/fd"/*; do
    [[ -e "$fd" || -L "$fd" ]] || continue
    target=$(readlink "$fd" 2>/dev/null || true)
    case "$target" in
        "$HOME/.local/share/goose"*|"$HOME/.config/goose"*)
            echo "ERROR: Process has open file descriptor pointing to production goose dir: $target (fd: $fd)" >&2
            exit 1
            ;;
    esac
done

echo "sandbox OK"
