#!/usr/bin/env bash
#
# Isolated goose sandbox launcher for honk.
#
# Launch as a systemd user unit (never a bare &):
#   systemctl --user stop honk-goose-sandbox 2>/dev/null
#   systemctl --user reset-failed honk-goose-sandbox 2>/dev/null
#   XDG_RUNTIME_DIR=/run/user/$(id -u) systemd-run --user --unit=honk-goose-sandbox --collect --setenv=PATH="$PATH" --setenv=HOME="$HOME" <abs path>/scripts/goose-sandbox.sh
#
# A second, isolated sandbox for a specific goose build (e.g. the fork):
#   systemd-run --user --unit=honk-goose-sandbox-patched --collect --setenv=PATH="$PATH" --setenv=HOME="$HOME" \
#     --setenv=HONK_SANDBOX="$HOME/.local/state/honk-sandbox-patched" --setenv=HONK_SANDBOX_PORT=4181 \
#     --setenv=HONK_SANDBOX_UNIT=honk-goose-sandbox-patched \
#     --setenv=GOOSE_BIN=<abs path to goose> <abs path>/scripts/goose-sandbox.sh
#
# Read logs:
#   journalctl --user -u honk-goose-sandbox

set -euo pipefail

RAW_SANDBOX="${HONK_SANDBOX:-$HOME/.local/state/honk-sandbox}"
case "$RAW_SANDBOX" in
    /*) ;;
    *)
        echo "ERROR: HONK_SANDBOX must be an absolute path: $RAW_SANDBOX" >&2
        exit 1
        ;;
esac

SANDBOX=$(realpath -m "$RAW_SANDBOX")
if [[ "$SANDBOX" == "$HOME" || "$SANDBOX" == "/" ]]; then
    echo "ERROR: SANDBOX cannot be \$HOME or root (/): $SANDBOX" >&2
    exit 1
fi

PORT="${HONK_SANDBOX_PORT:-4180}"
if ! [[ "$PORT" =~ ^[0-9]+$ ]]; then
    echo "ERROR: PORT must be numeric: $PORT" >&2
    exit 1
fi
if (( 10#$PORT == 4080 )); then
    echo "ERROR: Port 4080 is reserved for production goose-serve. Refusing port 4080." >&2
    exit 1
fi

# When GOOSE_BIN is set, require non-default HONK_SANDBOX, port, and unit
if [[ -n "${GOOSE_BIN:-}" ]]; then
    DEFAULT_SANDBOX_RESOLVED=$(realpath -m "$HOME/.local/state/honk-sandbox")
    if [[ -z "${HONK_SANDBOX:-}" || "$SANDBOX" == "$DEFAULT_SANDBOX_RESOLVED" ]]; then
        echo "ERROR: GOOSE_BIN is set, but HONK_SANDBOX is default ($SANDBOX). Set HONK_SANDBOX to an isolated path." >&2
        exit 1
    fi
    if [[ -z "${HONK_SANDBOX_PORT:-}" || "$PORT" == "4180" ]]; then
        echo "ERROR: GOOSE_BIN is set, but HONK_SANDBOX_PORT is default ($PORT). Set HONK_SANDBOX_PORT to an isolated port." >&2
        exit 1
    fi
    UNIT="${HONK_SANDBOX_UNIT:-}"
    if [[ -z "$UNIT" ]]; then
        cgroup_unit=$(sed -n 's|.*/\([^/]*\)\.\(service\|scope\)$|\1|p' /proc/self/cgroup 2>/dev/null || true)
        if [[ -n "$cgroup_unit" ]]; then
            UNIT="$cgroup_unit"
        fi
    fi
    if [[ -z "$UNIT" || "$UNIT" == "honk-goose-sandbox" || "$UNIT" == "honk-goose-sandbox.service" ]]; then
        echo "ERROR: GOOSE_BIN is set, but unit is default (${UNIT:-empty}). Set HONK_SANDBOX_UNIT or launch with a distinct systemd-run --unit." >&2
        exit 1
    fi
fi

mkdir -p "$SANDBOX"/{config,data,state}
chmod 0700 "$SANDBOX"

# First run only: build sandbox config.yaml from scratch using allowlist
if [[ ! -f "$SANDBOX/config/config.yaml" ]]; then
    src_config="$HOME/.config/goose/config.yaml"
    if [[ ! -f "$src_config" ]]; then
        echo "ERROR: Source configuration $src_config not found" >&2
        exit 1
    fi

    tmp_config=$(mktemp -p "$SANDBOX" config.XXXXXX)
    trap 'rm -f "$tmp_config"' EXIT

    # Allowlist: copy only these scalar/block keys from source if present
    awk '
    BEGIN {
        in_allow = 0
    }
    /^[A-Za-z0-9_]+:/ {
        in_allow = 0
        if (/^(GCP_VERTEX_HOST|GCP_AUTH_SCOPES|GOOSE_DISABLE_KEYRING|GOOSE_MODEL|GOOSE_PROVIDER):/) {
            in_allow = 1
            print
            next
        }
    }
    in_allow && /^[ \t]/ {
        print
        next
    }
    in_allow && !/^[ \t]/ {
        in_allow = 0
    }
    ' "$src_config" > "$tmp_config"

    # Append fixed lines and canonical extensions block
    cat >> "$tmp_config" << 'EOF'
GCP_VERTEX_SESSION_HEADER: x-honk-sandbox-session
GOOSE_TELEMETRY_ENABLED: false
extensions:
  developer:
    enabled: true
    type: platform
    name: developer
    description: Write and edit files, and execute shell commands
    display_name: Developer
    bundled: true
EOF

    # Post-generation assertions
    if grep -q 'plugins:' "$tmp_config"; then
        echo "ERROR: Assertion failed: generated config contains plugins:" >&2
        exit 1
    fi

    if grep -q 'x-opencode-session' "$tmp_config"; then
        echo "ERROR: Assertion failed: generated config contains x-opencode-session" >&2
        exit 1
    fi

    bad_ext=$(awk '
        /^extensions:/ { in_ext=1; next }
        in_ext && /^[A-Za-z0-9_]/ { in_ext=0 }
        in_ext && /^  [a-zA-Z0-9_-]+:/ && !/^  developer:/ { print $1 }
    ' "$tmp_config")
    if [[ -n "$bad_ext" ]]; then
        echo "ERROR: Assertion failed: generated config contains non-developer extension: $bad_ext" >&2
        exit 1
    fi

    install -m 0600 "$tmp_config" "$SANDBOX/config/config.yaml"
    rm -f "$tmp_config"
    trap - EXIT
fi

# Secret: create once directly at $SANDBOX/secret (mode 0600) under umask 077
if [[ ! -f "$SANDBOX/secret" ]]; then
    (
        umask 077
        if command -v openssl >/dev/null 2>&1; then
            openssl rand -hex 32 > "$SANDBOX/secret"
        else
            od -vN 32 -An -tx1 /dev/urandom | tr -d ' \n' > "$SANDBOX/secret"
        fi
    )
    chmod 0600 "$SANDBOX/secret"
fi

gcp_secret="/run/secrets/google_cloud_project"
if [[ ! -f "$gcp_secret" ]]; then
    echo "ERROR: GCP project secret file $gcp_secret not found" >&2
    exit 1
fi

gcloud_creds="$HOME/.config/gcloud/application_default_credentials.json"
if [[ ! -f "$gcloud_creds" ]]; then
    echo "ERROR: Google application default credentials $gcloud_creds not found" >&2
    exit 1
fi

# Which goose to run. Default: whatever `goose` is on PATH (the production build).
# Set GOOSE_BIN to an absolute path to run a specific build, e.g. a fork release.
GOOSE_BIN="${GOOSE_BIN:-goose}"
if [[ "$GOOSE_BIN" != "goose" ]]; then
    case "$GOOSE_BIN" in
        /*) ;;
        *)
            echo "ERROR: GOOSE_BIN must be an absolute path: $GOOSE_BIN" >&2
            exit 1
            ;;
    esac
    if [[ ! -x "$GOOSE_BIN" ]]; then
        echo "ERROR: GOOSE_BIN is not executable: $GOOSE_BIN" >&2
        exit 1
    fi
fi
goose_resolved=$(command -v "$GOOSE_BIN") || { echo "ERROR: $GOOSE_BIN not found" >&2; exit 1; }
echo "goose binary: $(readlink -f "$goose_resolved")" >&2

cd "$SANDBOX"
exec env \
    GOOSE_PATH_ROOT="$SANDBOX" \
    GOOSE_SERVER__SECRET_KEY="$(cat "$SANDBOX/secret")" \
    GOOSE_PROVIDER=gcp_vertex_ai \
    GOOSE_MODEL="${GOOSE_MODEL:-claude-opus-5-5}" \
    GCP_LOCATION=global \
    GCP_PROJECT_ID="$(cat "$gcp_secret")" \
    GOOGLE_APPLICATION_CREDENTIALS="$gcloud_creds" \
    "$GOOSE_BIN" serve --host 127.0.0.1 --port "$PORT"
