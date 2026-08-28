#!/bin/sh
# Size facts about the release binary and the dependency graph, as JSON.
# Run from the workspace root after `cargo build --release -p leviath-cli`.
set -e
BIN=${1:-target/release/lev}
bytes=$(wc -c < "$BIN" | tr -d ' ')
pkgs=$(cargo tree -p leviath-cli -e normal --prefix none 2>/dev/null | sort -u | wc -l | tr -d ' ')
# A 64-byte slice unique to the r50k/p50k tiktoken vocabularies: present means
# the encodings no reachable model selects are still linked in.
probe=$(python3 - "$BIN" <<'PY'
import glob, pathlib, sys
assets = glob.glob(str(pathlib.Path.home() / ".cargo/registry/src/*/tiktoken-rs-*/assets"))
if not assets:
    print("no-tiktoken-assets"); sys.exit()
a = pathlib.Path(sorted(assets)[-1])
r50 = a.joinpath("r50k_base.tiktoken").read_bytes()
cl = a.joinpath("cl100k_base.tiktoken").read_bytes()
o2 = a.joinpath("o200k_base.tiktoken").read_bytes()
probe = None
for off in range(100_000, len(r50) - 64, 977):
    p = r50[off:off + 64]
    if p not in cl and p not in o2:
        probe = p; break
if probe is None:
    print("no-probe"); sys.exit()
print("present" if probe in pathlib.Path(sys.argv[1]).read_bytes() else "absent")
PY
)
if command -v size >/dev/null 2>&1 && [ "$(uname)" = "Darwin" ]; then
  const=$(size -m "$BIN" | awk '/__const/ {print $NF; exit}')
else
  const=$(size -A "$BIN" 2>/dev/null | awk '/\.rodata/ {print $2; exit}')
fi
printf '{"binary":"%s","bytes":%s,"packages":%s,"const_bytes":"%s","r50k_vocab":"%s"}\n' \
  "$BIN" "$bytes" "$pkgs" "${const:-unknown}" "$probe"
