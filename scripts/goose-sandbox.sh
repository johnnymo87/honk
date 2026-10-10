#!/usr/bin/env bash
#
# Isolated goose sandbox launcher for honk.
#
# Launch as a systemd user unit (never a bare &):
#   systemctl --user stop honk-goose-sandbox 2>/dev/null
#   systemctl --user reset-failed honk-goose-sandbox 2>/dev/null
#   XDG_RUNTIME_DIR=/run/user/$(id -u) systemd-run --user --unit=honk-goose-sandbox --collect --setenv=PATH="$PATH" --setenv=HOME="$HOME" <abs path>/scripts/goose-sandbox.sh
#
# Read logs:
#   journalctl --user -u honk-goose-sandbox

set -euo pipefail

SANDBOX=$(realpath -m "${HONK_SANDBOX:-$HOME/.local/state/honk-sandbox}")
case "$SANDBOX" in
    /*) ;;
    *)
        echo "ERROR: SANDBOX path must be absolute: $SANDBOX" >&2
        exit 1
        ;;
esac

mkdir -p "$SANDBOX"/{config,data,state}
chmod 0700 "$SANDBOX"

# First run only: copy ONLY ~/.config/goose/config.yaml and edit it
if [[ ! -f "$SANDBOX/config/config.yaml" ]]; then
    src_config="$HOME/.config/goose/config.yaml"
    if [[ ! -f "$src_config" ]]; then
        echo "ERROR: Source configuration $src_config not found" >&2
        exit 1
    fi

    tmp_config=$(mktemp)
    awk '
    BEGIN {
        in_ext = 0
        in_dev = 0
        dev_found = 0
        header_seen = 0
        telem_seen = 0
    }
    /^GCP_PROJECT_ID:/ { next }
    /^GCP_VERTEX_SESSION_HEADER:/ {
        print "GCP_VERTEX_SESSION_HEADER: x-honk-sandbox-session"
        header_seen = 1
        next
    }
    /^GOOSE_TELEMETRY_ENABLED:/ {
        print "GOOSE_TELEMETRY_ENABLED: false"
        telem_seen = 1
        next
    }
    /^extensions:/ {
        in_ext = 1
        print "extensions:"
        next
    }
    in_ext && /^  developer:/ {
        in_dev = 1
        dev_found = 1
        print
        next
    }
    in_ext && in_dev && /^    / {
        print
        next
    }
    in_ext && in_dev && !/^    / {
        in_dev = 0
    }
    in_ext && /^[A-Za-z0-9_]/ {
        if (!dev_found) {
            print "  developer:"
            print "    enabled: true"
            print "    type: platform"
            print "    name: developer"
            print "    description: Write and edit files, and execute shell commands"
            print "    display_name: Developer"
            print "    bundled: true"
        }
        in_ext = 0
    }
    {
        if (!in_ext) {
            print
        }
    }
    END {
        if (in_ext && !dev_found) {
            print "  developer:"
            print "    enabled: true"
            print "    type: platform"
            print "    name: developer"
            print "    description: Write and edit files, and execute shell commands"
            print "    display_name: Developer"
            print "    bundled: true"
        }
        if (!header_seen) {
            print "GCP_VERTEX_SESSION_HEADER: x-honk-sandbox-session"
        }
        if (!telem_seen) {
            print "GOOSE_TELEMETRY_ENABLED: false"
        }
    }
    ' "$src_config" > "$tmp_config"

    install -m 0600 "$tmp_config" "$SANDBOX/config/config.yaml"
    rm -f "$tmp_config"
fi

# Secret: create once at $SANDBOX/secret (mode 0600)
if [[ ! -f "$SANDBOX/secret" ]]; then
    tmp_secret=$(mktemp)
    if command -v openssl >/dev/null 2>&1; then
        openssl rand -hex 32 > "$tmp_secret"
    else
        od -vN 32 -An -tx1 /dev/urandom | tr -d ' \n' > "$tmp_secret"
    fi
    install -m 0600 "$tmp_secret" "$SANDBOX/secret"
    rm -f "$tmp_secret"
fi

PORT="${HONK_SANDBOX_PORT:-4180}"
if [[ "$PORT" == "4080" ]]; then
    echo "ERROR: Port 4080 is reserved for production goose-serve. Refusing to run sandbox on port 4080." >&2
    exit 1
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

cd "$SANDBOX"
exec env \
    GOOSE_PATH_ROOT="$SANDBOX" \
    GOOSE_SERVER__SECRET_KEY="$(cat "$SANDBOX/secret")" \
    GOOSE_PROVIDER=gcp_vertex_ai \
    GCP_LOCATION=global \
    GCP_PROJECT_ID="$(cat "$gcp_secret")" \
    GOOGLE_APPLICATION_CREDENTIALS="$gcloud_creds" \
    goose serve --host 127.0.0.1 --port "$PORT"
