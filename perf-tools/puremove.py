#!/usr/bin/env python3
"""Pure-move check: the lines removed from ORIG (vs REF) must equal the
non-header lines of NEW, as multisets, after stripping whitespace, `pub(super) `
and `pub(crate) ` prefixes, and ignoring comments/blank/use/mod lines.
Usage: puremove.py REF ORIG NEW [NEW...]

REF is a git ref (origin/main), ORIG the file that shrank, NEW the files that
grew. Comments, blank, `use` and `mod` lines are ignored; `pub(super) `,
`pub(crate) ` and `pub(in ...) ` prefixes are stripped so a visibility widening
needed by the move does not read as a change."""
import subprocess, sys, collections, re
ref, orig, news = sys.argv[1], sys.argv[2], sys.argv[3:]
root = subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=True).stdout.strip()

def norm(line):
    l = line.strip()
    if not l or l.startswith("//") or l.startswith("use ") or l.startswith("mod ") or l.startswith("pub use") or l.startswith("pub(crate) use"):
        return None
    l = re.sub(r"^pub\(super\) ", "", l)
    l = re.sub(r"^pub\(crate\) ", "", l)
    l = re.sub(r"^pub\(in [^)]*\) ", "", l)
    return l

before = subprocess.run(["git", "-C", root, "show", f"{ref}:{orig}"], capture_output=True, text=True).stdout.split("\n")
after = open(f"{root}/{orig}").read().split("\n")
removed = collections.Counter(filter(None, map(norm, before)))
removed.subtract(collections.Counter(filter(None, map(norm, after))))
removed = +removed  # lines the original lost
added = collections.Counter()
for n in news:
    added.update(filter(None, map(norm, open(f"{root}/{n}").read().split("\n"))))
# The new files carry their own `impl X {` / `}` wrappers; drop one of each per file.
for n in news:
    for w in ("}", ):
        if added[w] > 0:
            added[w] -= 1
    txt = open(f"{root}/{n}").read()
    for m in re.finditer(r"^impl \w+ \{$", txt, re.M):
        added[m.group(0)] -= 1
added = +added
only_removed = +(removed - added)
only_added = +(added - removed)
print(f"{orig}: removed {sum(removed.values())} lines, new files carry {sum(added.values())}")
for l, c in list(only_removed.items())[:15]:
    print(f"  missing in new files x{c}: {l[:100]}")
for l, c in list(only_added.items())[:15]:
    print(f"  extra in new files   x{c}: {l[:100]}")
print("  PURE" if not only_removed and not only_added else "  DIFFERS")
