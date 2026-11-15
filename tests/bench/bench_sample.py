#!/usr/bin/env python3
"""Sample a joint Core + e-lite IBD run and write CSV + summary.

The feeder is a separate regtest node; the syncer Core is started by e-lite
(`manage = managed`), so this exercises the real pause/index/prune pipeline.

Measurement rules enforced here (they were violated by the first version):

* every timestamp is elapsed-monotonic, never an absolute clock mixed with a
  relative one;
* CPU percentages use the real `CLK_TCK`, not a hardcoded 100 Hz;
* peak RSS is the peak of the *simultaneous* sum, not the sum of two peaks;
* a run that never reaches the tip exits non-zero, so the caller cannot
  mistake a timeout for a result;
* units in the header match the columns.
"""
import argparse
import json
import os
import shlex
import subprocess
import sys
import time
import urllib.request

CLK_TCK = os.sysconf("SC_CLK_TCK") if hasattr(os, "sysconf") else 100


def status(url):
    try:
        with urllib.request.urlopen(url, timeout=2) as r:
            return json.load(r)
    except Exception:
        return {}


def core_info(cli):
    try:
        out = subprocess.check_output(cli, stderr=subprocess.DEVNULL, timeout=5)
        info = json.loads(out)
    except Exception:
        return {}
    # `getnetworkinfo` is a second call; keep it cheap and best-effort.
    try:
        net = subprocess.check_output(
            cli[:-1] + ["getnetworkinfo"], stderr=subprocess.DEVNULL, timeout=5
        )
        n = json.loads(net)
        info["networkactive"] = n.get("networkactive")
        info["connections"] = n.get("connections")
    except Exception:
        pass
    return info


def rss_kib(pid):
    if not pid:
        return 0
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except OSError:
        pass
    return 0


def cpu_ticks(pid):
    if not pid:
        return 0
    try:
        with open(f"/proc/{pid}/stat") as f:
            parts = f.read().split()
        return int(parts[13]) + int(parts[14])
    except (OSError, IndexError):
        return 0


def du(path):
    """Apparent size of regular files. Directories may nest; callers must
    avoid summing overlapping trees (the index excludes the Core datadir)."""
    total = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            try:
                total += os.stat(os.path.join(root, name)).st_size
            except OSError:
                pass
    return total


def pid_from_file(path):
    try:
        with open(path) as f:
            return int(f.read().split()[0])
    except Exception:
        return 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--status-url", required=True)
    ap.add_argument(
        "--core-cli",
        required=True,
        help="bitcoin-cli command as one quoted string",
    )
    ap.add_argument("--core-datadir", required=True)
    ap.add_argument("--data-dir", required=True)
    ap.add_argument("--core-pid-file", required=True)
    ap.add_argument("--elite-pid-file", required=True)
    ap.add_argument("--tip", type=int, required=True)
    ap.add_argument("--csv", required=True)
    ap.add_argument("--summary", required=True)
    ap.add_argument("--timeout", type=int, default=3600)
    ap.add_argument("--interval", type=float, default=0.5)
    args = ap.parse_args()
    core_cli = shlex.split(args.core_cli)

    start = time.monotonic()
    last_core = 0.0
    core = {}
    tip_seen_at = None
    index_seen_at = None
    paused_since = None
    paused_total = 0.0
    max_rss_core = max_rss_elite = 0
    # Peak of the simultaneous sum, not the sum of independent peaks.
    max_rss_combined = 0
    max_disk = 0
    max_pending = 0
    max_compactions = 0
    samples = 0
    last_ticks = {"core": 0, "elite": 0}
    last_ticks_at = {"core": 0.0, "elite": 0.0}
    cpu_pct = {"core": 0.0, "elite": 0.0}
    last_indexed = 0
    last_txnum = 0

    with open(args.csv, "w") as out:
        out.write(
            "t_s,state,indexed,core,lag,networkactive,ready,core_ibd,"
            "rss_elite_kib,rss_core_kib,rss_combined_kib,disk_bytes,"
            "pending_compaction_bytes,running_compactions,commits,committed_txs,"
            "total_read_us,total_decode_us,total_prefetch_us,total_wait_us,"
            "total_apply_us,total_commit_us,commit_txids_us,commit_db_us,"
            "cpu_elite_pct,cpu_core_pct\n"
        )
        while time.monotonic() - start < args.timeout:
            now = time.monotonic() - start
            st = status(args.status_url)
            if now - last_core >= 2.0:
                core = core_info(core_cli)
                last_core = now

            elite_pid = pid_from_file(args.elite_pid_file)
            core_pid = pid_from_file(args.core_pid_file)
            re_ = rss_kib(elite_pid)
            rc = rss_kib(core_pid)
            combined = re_ + rc
            max_rss_elite = max(max_rss_elite, re_)
            max_rss_core = max(max_rss_core, rc)
            max_rss_combined = max(max_rss_combined, combined)
            disk = du(args.data_dir) + du(args.core_datadir)
            max_disk = max(max_disk, disk)

            rb = st.get("rocksdb", {}) or {}
            stages = st.get("stage_totals_us", {}) or {}
            pending = rb.get("pending_compaction_bytes", 0)
            compactions = rb.get("running_compactions", 0)
            max_pending = max(max_pending, pending)
            max_compactions = max(max_compactions, compactions)

            for key, pid in (("core", core_pid), ("elite", elite_pid)):
                ticks = cpu_ticks(pid)
                dt = now - last_ticks_at[key]
                if ticks >= last_ticks[key] and dt > 0:
                    cpu_pct[key] = 100.0 * (ticks - last_ticks[key]) / (CLK_TCK * dt)
                last_ticks[key] = ticks
                last_ticks_at[key] = now

            state = st.get("state", "")
            indexed = int(st.get("indexed_height", 0) or 0)
            txnum = int(st.get("txnum", 0) or 0)
            last_indexed = indexed
            last_txnum = txnum
            core_h = int(core.get("blocks", 0) or 0)
            if core_h >= args.tip and tip_seen_at is None:
                tip_seen_at = now
            if indexed >= args.tip and index_seen_at is None:
                index_seen_at = now
            if state == "PAUSE_DOWNLOAD":
                if paused_since is None:
                    paused_since = now
            elif paused_since is not None:
                paused_total += now - paused_since
                paused_since = None

            out.write(
                f"{now:.3f},{state},{indexed},{core_h},"
                f"{max(core_h - indexed, 0)},{int(bool(core.get('networkactive', False)))},"
                f"{int(bool(st.get('ready', False)))},{int(bool(core.get('initialblockdownload', False)))},"
                f"{re_},{rc},{combined},{disk},{pending},{compactions},"
                f"{st.get('commits', 0)},{st.get('committed_txs', 0)},"
                f"{stages.get('read', 0)},{stages.get('decode', 0)},{stages.get('prefetch', 0)},"
                f"{stages.get('wait', 0)},{stages.get('apply', 0)},{stages.get('commit', 0)},"
                f"{(st.get('commit_breakdown_us', {}) or {}).get('txids_append_fsync', 0)},"
                f"{(st.get('commit_breakdown_us', {}) or {}).get('rocksdb_write', 0)},"
                f"{cpu_pct['elite']:.1f},{cpu_pct['core']:.1f}\n"
            )
            out.flush()
            samples += 1
            if indexed >= args.tip and core_h >= args.tip and state in ("READY", "FULLY SYNCED"):
                break
            time.sleep(args.interval)

    if paused_since is not None:
        paused_total += (time.monotonic() - start) - paused_since

    st = status(args.status_url)
    reached = last_indexed >= args.tip
    summary = {
        "tip": args.tip,
        "samples": samples,
        "completed": reached,
        "total_seconds": round(time.monotonic() - start, 3),
        "core_tip_seconds": round(tip_seen_at, 3) if tip_seen_at else None,
        "index_tip_seconds": round(index_seen_at, 3) if index_seen_at else None,
        "paused_seconds": round(paused_total, 3),
        "max_rss_core_kib": max_rss_core,
        "max_rss_elite_kib": max_rss_elite,
        "max_combined_rss_kib": max_rss_combined,
        "max_disk_bytes": max_disk,
        "max_pending_compaction_bytes": max_pending,
        "max_running_compactions": max_compactions,
        "final_indexed": last_indexed,
        "final_txnum": last_txnum,
        "commits": st.get("commits", 0),
        "committed_txs": st.get("committed_txs", 0),
        "stage_totals_us": st.get("stage_totals_us", {}),
        "commit_breakdown_us": st.get("commit_breakdown_us", {}),
        "final_state": st.get("state", ""),
        "prune_requests": st.get("prune_requests", 0),
        "prune_failures": st.get("prune_failures", 0),
    }
    with open(args.summary, "w") as f:
        json.dump(summary, f, indent=2)
    print(json.dumps(summary, indent=2))
    if not reached:
        print(
            f"INCOMPLETE: indexed {last_indexed} of {args.tip} in "
            f"{summary['total_seconds']}s",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
