#!/bin/sh
# Starts bman from its folder (so it finds music/ and config.txt there),
# passing along any options, e.g.:
#   ./start-bman.sh
#   ./start-bman.sh --terminal
#   ./start-bman.sh --driver window
# The folder is ~/bman unless BMAN_DIR says otherwise.

BMAN_DIR="${BMAN_DIR:-$HOME/bman}"

if [ ! -x "$BMAN_DIR/bman" ]; then
    echo "bman isn't installed in $BMAN_DIR (run install.sh, or set BMAN_DIR)." >&2
    exit 1
fi

cd "$BMAN_DIR" || exit 1
exec ./bman "$@"
