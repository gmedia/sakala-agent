#!/bin/sh
# Railpack looks for mise at /tmp/railpack/mise/mise-<version>, and downloads it,
# unchecked, if it is not there. /tmp is an empty tmpfs at run time, so link the
# baked copy into place. A deadline here ends the container even if the `docker
# run` client that started it is gone.
#
# The Agent gets the result from stdout, not from a file, so the sandbox needs no
# host path it can write to. Railpack's own output goes to stderr, which is logged.
set -eu
if [ "${1:-}" = "--run" ]; then
    shift
    "$@" >&2
    head -c 1048576 "$SANDBOX_RESULT_FILE" | base64
    exit 0
fi
mkdir -p /tmp/railpack/mise
ln -s "/usr/local/bin/mise-${MISE_VERSION}" "/tmp/railpack/mise/mise-${MISE_VERSION}"
exec timeout -s KILL "${SANDBOX_TIMEOUT_SECONDS:-300}" "$0" --run "$@"
