#!/bin/sh
# Wrap a command with the OS's own counters and print them.
#   Linux: perf stat (instructions, task-clock, page faults) + strace -c
#   macOS: /usr/bin/time -l (rusage incl. max RSS)
# The gate numbers in the plan come from Linux; macOS is corroboration.
set -e
case "$(uname)" in
  Linux)
    if command -v perf >/dev/null 2>&1; then
      perf stat -e instructions,task-clock,page-faults -- "$@"
    else
      /usr/bin/time -v "$@"
    fi
    if command -v strace >/dev/null 2>&1; then
      strace -c -f -e trace=openat,newfstatat,statx,read,pread64 -o /tmp/measure.strace "$@" >/dev/null
      cat /tmp/measure.strace
    fi
    ;;
  Darwin)
    /usr/bin/time -l "$@"
    ;;
esac
