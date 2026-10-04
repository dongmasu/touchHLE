#!/bin/sh
set -eu

if [ -z "${TOUCHHLE_GOOGLE_DESKTOP_OAUTH_CLIENT_ID:-}" ]; then
    printf '%s\n' \
        "Set TOUCHHLE_GOOGLE_DESKTOP_OAUTH_CLIENT_ID in the environment before building." \
        >&2
    exit 1
fi
if [ -z "${TOUCHHLE_GOOGLE_DESKTOP_OAUTH_CLIENT_SECRET:-}" ]; then
    printf '%s\n' \
        "Set TOUCHHLE_GOOGLE_DESKTOP_OAUTH_CLIENT_SECRET in the environment before building." \
        >&2
    exit 1
fi

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd "$SCRIPT_DIR/.." && pwd)
cd "$REPO_ROOT"

CMAKE_POLICY_VERSION_MINIMUM=3.5 cargo build --release
