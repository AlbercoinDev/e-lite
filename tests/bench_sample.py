#!/usr/bin/env python3
"""Sample a joint Core + e-lite IBD run and write CSV + summary.

The feeder is a separate regtest node; the syncer Core is started by e-lite
(`manage = managed`), so this exercises the real pause/index/prune pipeline.
"""
import argparse
import json
import shlex
import os
import subprocess
import sys
import time
import urllib.request


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
            return int(f.read().strip())
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
    max_disk = 0
    max_pending = 0
    max_compactions = 0
    samples = 0
    last_ticks = {"core": 0, "elite": 0}
    last_ticks_at = {"core": start, "elite": start}
    cpu_pct = {"core": 0.0, "elite": 0.0}

    with open(args.csv, "w") as out:
        out.write(
            "t,state,indexed,core,lag,network,synced,core_ibd,"
            "rss_elite_kib,rss_core_kib,disk_bytes,pending_compaction,"
            "running_compactions,commits,committed_txs,"
            "stage_read_ms,stage_decode_ms,stage_prefetch_ms,stage_wait_ms,"
            "stage_apply_us,stage_commit_us,commit_txids_us,commit_db_us,"
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
            re = rss_kib(elite_pid)
            rc = rss_kib(core_pid)
            max_rss_elite = max(max_rss_elite, re)
            max_rss_core = max(max_rss_core, rc)
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
                    cpu_pct[key] = 100.0 * (ticks - last_ticks[key]) / (100.0 * dt)
                last_ticks[key] = ticks
                last_ticks_at[key] = now

            state = st.get("state", "")
            indexed = int(st.get("indexed_height", 0) or 0)
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
                f"{re},{rc},{disk},{pending},{compactions},"
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
    summary = {
        "tip": args.tip,
        "samples": samples,
        "total_seconds": round(time.monotonic() - start, 3),
        "core_tip_seconds": round(tip_seen_at, 3) if tip_seen_at else None,
        "index_tip_seconds": round(index_seen_at, 3) if index_seen_at else None,
        "paused_seconds": round(paused_total, 3),
        "max_rss_core_kib": max_rss_core,
        "max_rss_elite_kib": max_rss_elite,
        "max_combined_rss_kib": max_rss_core + max_rss_elite,
        "max_disk_bytes": max_disk,
        "max_pending_compaction_bytes": max_pending,
        "max_running_compactions": max_compactions,
        "commits": st.get("commits", 0),
        "committed_txs": st.get("committed_txs", 0),
        "stage_totals_ms": st.get("stage_totals_ms", {}),
        "final_state": st.get("state", ""),
        "final_indexed": st.get("indexed_height", 0),
        "prune_requests": st.get("prune_requests", 0),
        "prune_failures": st.get("prune_failures", 0),
    }
    with open(args.summary, "w") as f:
        json.dump(summary, f, indent=2)
    print(json.dumps(summary, indent=2))
    return 0 if summary["final_indexed"] and int(summary["final_indexed"]) >= args.tip else 1


if __name__ == "__main__":
    sys.exit(main())
