#!/usr/bin/env python3
"""Run a command, capture stdout/stderr to a log and report time + max RSS.

usage: bench_run.py LOG META CMD [ARGS...]
META receives "seconds max_rss_kib" when the child exits.
"""
import os
import resource
import subprocess
import sys
import time

log, meta = sys.argv[1], sys.argv[2]
cmd = sys.argv[3:]
if cmd and cmd[0] == "--":
    cmd = cmd[1:]
start = time.monotonic()
with open(log, "wb") as out:
    rc = subprocess.call(cmd, stdout=out, stderr=subprocess.STDOUT)
elapsed = time.monotonic() - start
rss = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
with open(meta, "w") as fh:
    fh.write(f"{elapsed:.3f} {rss}\n")
sys.exit(rc)
