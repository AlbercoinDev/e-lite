#!/usr/bin/env python3
"""Offline A/B of two published e-lite binaries. Requires Python 3 and bitcoind.

Usage: python3 tests/bench/bench_provider_ab.py OLD NEW --output /tmp/opencode/results --runs 3
All services bind loopback. No Tor daemon, production index, or private wallet.
"""

import argparse
import asyncio
import base64
from collections import Counter
from contextlib import closing
import hashlib
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import itertools
import json
from pathlib import Path
import shutil
import socket
import statistics
import subprocess
import threading
import time
import urllib.request


HOST = "127.0.0.1"
OPERATORS = [f"{letter * 56}.onion" for letter in "abcde"]
N_TX = 239
AUTH = "Basic " + base64.b64encode(b"ab:throwaway-regtest").decode()


def free_port():
    with closing(socket.socket()) as sock:
        sock.bind((HOST, 0))
        return sock.getsockname()[1]


def rpc(port, payload):
    con = HTTPConnection(HOST, port, timeout=150)
    try:
        con.request("POST", "/", json.dumps(payload), {
            "Authorization": AUTH, "Content-Type": "application/json",
        })
        return json.loads(con.getresponse().read())
    finally:
        con.close()


def call(port, method, *params):
    response = rpc(port, {"jsonrpc": "2.0", "id": 0, "method": method, "params": params})
    if response.get("error"):
        raise RuntimeError(response)
    return response["result"]


def poll(fn, timeout=45):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            value = fn()
            if value:
                return value
        except (OSError, RuntimeError, ValueError):
            pass
        time.sleep(0.05)
    raise RuntimeError(f"readiness timeout after {timeout}s")


def write_config(folder, index, core, electrum, metrics, socks=None):
    conf = folder / "e-lite.toml"
    conf.write_text(f"""[network]
name = "regtest"
data_dir = "{index}"
[core]
manage = "external"
rpc_url = "http://{HOST}:{core}"
rpc_user = "ab"
rpc_password = "throwaway-regtest"
[index]
batch_blocks = 16
[pruning]
min_free_gib = 1
[server]
listen = "{HOST}:{electrum}"
tls = false
rate_limit = 0
[metrics]
listen = "{HOST}:{metrics}"
[tor]
mode = "off"
[logging]
level = "debug"
[rawtx]
enabled = {str(socks is not None).lower()}
use_p2p = false
use_esplora = false
use_tor = true
allow_clearnet = false
tor_streams = 8
tor_socks = "{HOST}:{socks or 1}"
wallet_deadline_seconds = 45
cache_mib = 64
""")
    (folder / "public-servers.toml").write_text("[regtest]\nesplora = []\nelectrum = " +
        json.dumps([f"{host}:50001:t" for host in OPERATORS]) + "\n")
    # Same baseline in both; three decoys and at least three operators.
    (folder / "privacy.toml").write_text('preset = "balanced"\n')
    return conf


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=25)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def build_fixture(folder, old, core_port):
    print("Building synthetic regtest chain and shared index", flush=True)
    def address(tag):
        # Public deterministic scripts, independent of wallets.
        return call(core_port, "decodescript", f"0014{tag:040x}")["address"]

    fixture = call(core_port, "generatetoaddress", 16, address(1))
    scripts = [address(i) for i in range(2, 6)]
    wallet = []
    # Overlapping ages for the three decoy histories, 239 rows each.
    for n in [60, 60, 60, 59]:
        for i, script in enumerate(scripts):
            blocks = call(core_port, "generatetoaddress", n, script)
            fixture.extend(blocks)
            if i == 0:
                wallet.extend(blocks)
    # Tip is not a wallet block; all 239 wallet heights are reported pruned.
    fixture.extend(call(core_port, "generatetoaddress", 1, address(9)))
    index = folder / "base-index"
    conf = write_config(folder, index, core_port, free_port(), free_port())
    with (folder / "index.log").open("w") as log:
        subprocess.run([old, "index", "--config", conf, "--no-follow"], stdout=log,
                       stderr=subprocess.STDOUT, check=True, timeout=150)
    raw = {}
    wallet_txids = set()
    samples = set()
    heights = {}
    for start in range(0, len(fixture), 24):
        requests = [{"jsonrpc": "2.0", "id": idx, "method": "getblock", "params": [h, 2]}
                    for idx, h in enumerate(fixture[start:start + 24], start=start)]
        for reply in rpc(core_port, requests):
            block = reply["result"]
            heights[block["hash"]] = block["height"]
            for tx in block["tx"]:
                raw[tx["txid"]] = tx["hex"]
                if block["hash"] in wallet:
                    wallet_txids.add(tx["txid"])
                if block["height"] <= 4:
                    samples.add(tx["txid"])
    assert len(wallet_txids) == N_TX and len(samples) == 4
    wallet_hash = hashlib.sha256(bytes.fromhex(f"0014{2:040x}")).digest()[::-1].hex()
    return index, raw, wallet_txids, samples, wallet_hash, heights, len(fixture)


def prune_shim(core_port, heights, tip):
    """Proxy full regtest Core, but report everything below tip as pruned."""
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            response = rpc(core_port, payload)
            def apply(request, reply):
                if request["method"] == "getblockchaininfo" and reply.get("result"):
                    reply["result"].update(pruned=True, pruneheight=tip, automatic_pruning=False)
                if request["method"] == "getblock" and heights.get(request["params"][0], tip) < tip:
                    return {"id": request["id"], "error": {"code": -1, "message": "Block not available (pruned data)"}}
                return reply
            if isinstance(payload, list):
                by_id = {request["id"]: request for request in payload}
                response = [apply(by_id[reply["id"]], reply) for reply in response]
            else:
                response = apply(payload, response)
            data = json.dumps(response).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    server = ThreadingHTTPServer((HOST, 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


class Archive:
    def __init__(self, raw, wallet, samples, broken):
        self.raw = raw
        self.wallet = wallet
        self.samples = samples
        self.broken = broken
        self.streams = []

    async def client(self, reader, writer):
        entry = None
        try:
            version, n = await reader.readexactly(2)
            methods = await reader.readexactly(n)
            assert version == 5 and 2 in methods
            writer.write(b"\x05\x02")
            version, n = await reader.readexactly(2)
            assert version == 1
            await reader.readexactly(n)
            await reader.readexactly((await reader.readexactly(1))[0])
            writer.write(b"\x01\x00")
            assert await reader.readexactly(4) == b"\x05\x01\x00\x03"
            host = (await reader.readexactly((await reader.readexactly(1))[0])).decode()
            assert host in OPERATORS
            assert await reader.readexactly(2) == (50001).to_bytes(2, "big")
            entry = {"operator": host[0], "probe": False, "items": 0, "real": 0, "failed": False}
            self.streams.append(entry)
            writer.write(b"\x05\x00\x00\x01\x7f\x00\x00\x01\x00\x00")
            ver = json.loads(await reader.readline())
            assert ver["method"] == "server.version"
            writer.write((json.dumps({"id": ver["id"], "result": ["Fixture", "1.4"]}) + "\n").encode())
            await writer.drain()
            batch = json.loads(await reader.readline())
            assert isinstance(batch, list) and all(r["method"] == "blockchain.transaction.get" for r in batch)
            ids = [r["params"][0] for r in batch]
            entry.update(probe=set(ids) == self.samples, items=len(ids),
                         real=sum(txid in self.wallet for txid in ids))
            if host in self.broken:
                entry["failed"] = True
                # No bytes for the tx batch: the real fetcher's 3s deadline
                # or the qualification worker's 2.5s deadline controls this.
                await asyncio.wait_for(reader.read(), 6)
            else:
                await asyncio.sleep(0.075)
                result = [{"id": r["id"], "result": self.raw[txid]} for r, txid in zip(batch, ids)]
                writer.write((json.dumps(result) + "\n").encode())
                await writer.drain()
        except (ConnectionError, asyncio.IncompleteReadError, asyncio.TimeoutError, json.JSONDecodeError):
            if entry is not None and entry["items"] == 0:
                entry["incomplete"] = True
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except OSError:
                pass


def status(port):
    with urllib.request.urlopen(f"http://{HOST}:{port}/status", timeout=2) as response:
        return json.load(response)


async def load_wallet(port, script, expected, raw):
    reader, writer = await asyncio.open_connection(HOST, port)
    serial = itertools.count()

    async def batch(method, params):
        reqs = [{"id": next(serial), "jsonrpc": "2.0", "method": method, "params": arg} for arg in params]
        writer.write((json.dumps(reqs if len(reqs) > 1 else reqs[0]) + "\n").encode())
        await writer.drain()
        line = await asyncio.wait_for(reader.readline(), 55)
        if not line:
            raise RuntimeError("wallet connection closed with a transaction outstanding")
        replies = json.loads(line)
        if isinstance(replies, dict):
            replies = [replies]
        response = {r["id"]: r for r in replies}
        return [response[r["id"]]["result"] for r in reqs]

    started = time.monotonic()
    try:
        await batch("server.version", [["Fixture Sparrow-like", "1.4.2"]])
        await batch("blockchain.scripthash.subscribe", [[script]])
        history, = await batch("blockchain.scripthash.get_history", [[script]])
        ordered = [row["tx_hash"] for row in history]
        assert len(ordered) == N_TX and set(ordered) == expected
        history_ms = (time.monotonic() - started) * 1000
        # Typical JSON-RPC batches, kept deterministic across versions.
        first = None
        for offset in range(0, len(ordered), 32):
            chunk = ordered[offset:offset + 32]
            got = await batch("blockchain.transaction.get", [[txid] for txid in chunk])
            assert got == [raw[txid] for txid in chunk]
            if first is None:
                first = (time.monotonic() - started) * 1000
        return {"history_ms": round(history_ms), "first_batch_ms": round(first),
                "total_ms": round((time.monotonic() - started) * 1000), "verified": N_TX}
    finally:
        writer.close()
        await writer.wait_closed()


async def run_once(path, version, binary, scenario, core, index, raw, wallet, samples, script, tip):
    path.mkdir()
    dest = path / "index"
    shutil.copytree(index, dest)
    (dest / "decoy.key").write_bytes(bytes(range(32)))
    archive = Archive(raw, wallet, samples, set(OPERATORS[1:3]) if scenario != "healthy" else set())
    proxy = await asyncio.start_server(archive.client, HOST, 0, limit=2**22)
    socks = proxy.sockets[0].getsockname()[1]
    electrum, metrics = free_port(), free_port()
    conf = write_config(path, dest, core, electrum, metrics, socks)
    run_start = time.monotonic()
    with (path / "server.log").open("w") as log:
        proc = subprocess.Popen([binary, "serve", "--config", conf], stdout=log, stderr=subprocess.STDOUT)
        try:
            await asyncio.to_thread(poll, lambda: (doc if (doc := status(metrics)).get("state") == "READY"
                                                    and doc.get("indexed_height") == tip else None))
            startup_ms = round((time.monotonic() - run_start) * 1000)
            if scenario == "degraded-settled":
                # Both start with empty transaction cache and have the same
                # lead time. Only providers have been allowed to warm up.
                await asyncio.sleep(7)
            before = await asyncio.to_thread(status, metrics)
            assert before["rawtx"]["cache_txs"] == 0, before["rawtx"]
            result = await load_wallet(electrum, script, wallet, raw)
            await asyncio.sleep(0.2)
            after = await asyncio.to_thread(status, metrics)
        finally:
            await asyncio.to_thread(stop, proc)
            proxy.close()
            await proxy.wait_closed()
    sent, real = Counter(), Counter()
    streams = [row for row in archive.streams if row["items"] and not row["probe"]]
    probes = [row for row in archive.streams if row["probe"]]
    for row in streams:
        sent[row["operator"]] += row["items"]
        real[row["operator"]] += row["real"]
    failed = sum(row["failed"] for row in streams)
    result.update(version=version, scenario=scenario, startup_ms=startup_ms,
        failed_wallet_streams=failed, wallet_streams=len(streams), probe_streams=len(probes),
        failed_probe_streams=sum(row["failed"] for row in probes),
        sent_by_operator=dict(sorted(sent.items())), real_by_operator=dict(sorted(real.items())),
        largest_real_share=round(max(real.values()) / sum(real.values()) * 100, 1),
        tor_peak=after["rawtx"]["tor_governor"]["peak"],
        qualification_at_load=before["rawtx"].get("qualification"),
        qualification_after=after["rawtx"].get("qualification"))
    (path / "streams.json").write_text(json.dumps(archive.streams, indent=2) + "\n")
    (path / "status.json").write_text(json.dumps(after["rawtx"], indent=2) + "\n")
    print(json.dumps(result, sort_keys=True), flush=True)
    return result


async def main(args):
    folder = args.output.resolve()
    folder.mkdir()
    core_dir = folder / "core"
    core_dir.mkdir()
    port = free_port()
    old, new = str(args.old.resolve()), str(args.new.resolve())
    v_old, v_new = [subprocess.check_output([binary, "--version"], text=True).strip() for binary in (old, new)]
    with (folder / "core.log").open("w") as log:
        core = subprocess.Popen(["bitcoind", "-regtest", f"-datadir={core_dir}", "-listen=0",
            "-connect=0", "-dnsseed=0", "-server", "-rpcuser=ab", "-rpcpassword=throwaway-regtest",
            f"-rpcport={port}", "-txindex=0", "-printtoconsole"], stdout=log, stderr=subprocess.STDOUT)
        shim = None
        try:
            poll(lambda: call(port, "getblockcount") == 0)
            index, raw, wallet, samples, script, heights, tip = await asyncio.to_thread(build_fixture, folder, old, port)
            shim = prune_shim(port, heights, tip)
            all_results = []
            for scenario in args.scenario:
                for n in range(args.runs):
                    versions = [("old", v_old, old), ("new", v_new, new)]
                    if n % 2:
                        versions.reverse()
                    for key, version, binary in versions:
                        result = await run_once(folder / f"{scenario}-{n + 1}-{key}", version, binary, scenario,
                            shim.server_port, index, raw, wallet, samples, script, tip)
                        all_results.append(result)
            (folder / "results.json").write_text(json.dumps(all_results, indent=2) + "\n")
            for scenario in args.scenario:
                for version in (v_old, v_new):
                    matching = [r for r in all_results if (r["scenario"], r["version"]) == (scenario, version)]
                    fields = ("first_batch_ms", "total_ms", "failed_wallet_streams", "wallet_streams",
                              "probe_streams", "failed_probe_streams", "largest_real_share")
                    print(scenario, version, {k: statistics.median(r[k] for r in matching) for k in fields})
        finally:
            if shim is not None:
                shim.shutdown()
                shim.server_close()
            stop(core)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("old", type=Path)
    parser.add_argument("new", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--scenario", choices=["healthy", "degraded-cold", "degraded-settled"],
                        nargs="+", default=["healthy", "degraded-cold", "degraded-settled"])
    args = parser.parse_args()
    if args.runs < 1:
        parser.error("--runs must be at least 1")
    asyncio.run(main(args))
