#!/bin/sh
# The isolated environment every live probe runs in. Source it, or use it as
# a wrapper: `harness.sh lev ps`.
#
#   LEVIATH_HOME   short path (the Unix control socket has a ~104-byte limit)
#   LEVIATH_SKIP_DOTENV  the repo-root .env holds a real key; never load it
#   OPENAI_*       point the native OpenAI provider at mock.py
#   LV_RUNS_DIR    optional; forwarded as LEVIATH_RUNS_DIR (the fake-runs corpus)
#   LV_MOCK_OVERSIZE_MIB  optional; forwarded to mock.py, which then answers
#                  every completion with one frame that never closes
#
# Nothing here reads the caller's environment: `env -i` first, then exactly
# these names, so a probe cannot accidentally reach a real provider.
: "${LV_HOME:=/tmp/lv}"
: "${LV_MOCK_PORT:=8099}"
: "${LV_BIN:=$(cd "$(dirname "$0")/.." && pwd)/target/release/lev}"
: "${LEVIATH_API_TOKEN:=probe-token}"

mkdir -p "$LV_HOME/.leviath/agents" "$LV_HOME/work"
if [ ! -e "$LV_HOME/.leviath/agents/probe" ]; then
  cp -R "$(dirname "$0")/agents/probe" "$LV_HOME/.leviath/agents/probe"
fi

exec env -i \
  PATH="/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin" \
  HOME="$LV_HOME" \
  LEVIATH_HOME="$LV_HOME" \
  LEVIATH_SKIP_DOTENV=1 \
  LEVIATH_API_TOKEN="$LEVIATH_API_TOKEN" \
  OPENAI_API_KEY=mock \
  OPENAI_BASE_URL="http://127.0.0.1:$LV_MOCK_PORT/v1" \
  TERM="${TERM:-xterm-256color}" \
  ${LV_RUNS_DIR:+LEVIATH_RUNS_DIR="$LV_RUNS_DIR"} \
  ${LV_MOCK_OVERSIZE_MIB:+LV_MOCK_OVERSIZE_MIB="$LV_MOCK_OVERSIZE_MIB"} \
  LV_MOCK_PORT="$LV_MOCK_PORT" \
  ${LV_SERVE_PORT:+LV_SERVE_PORT="$LV_SERVE_PORT"} \
  LV_BIN="$LV_BIN" \
  "$@"
