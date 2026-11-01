#!/usr/bin/env python3
"""Sparrow-like connect sequence: fees, broadcast, mempool, notifications.

Usage: e2e_sparrow.py <electrum_port> <core_datadir> <rpc_port> <mine_address>
"""
import json
import socket
import subprocess
import sys
import time

port, core, rpc, addr = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]


def cli(*args):
    out = subprocess.run(
        ["bitcoin-cli", "-regtest", f"-datadir={core}", f"-rpcport={rpc}", *args],
        check=True,
        capture_output=True,
        text=True,
    )
    return out.stdout.strip()


def sh_of(address):
    import hashlib

    script = bytes.fromhex(json.loads(cli("getaddressinfo", address))["scriptPubKey"])
    return hashlib.sha256(script).digest()[::-1].hex()


s = socket.create_connection(("127.0.0.1", port), timeout=20)
f = s.makefile("rwb")


def call(i, m, p):
    f.write((json.dumps({"jsonrpc": "2.0", "id": i, "method": m, "params": p}) + "\n").encode())
    f.flush()
    r = json.loads(f.readline())
    assert "error" not in r, (m, r)
    return r["result"]


# Connect sequence used by Sparrow.
assert call(1, "server.version", ["Sparrow", "1.4"])[1] == "1.4"
call(2, "server.features", [])
call(3, "blockchain.headers.subscribe", [])
call(4, "server.banner", [])
est = call(5, "blockchain.estimatefee", [6])
assert isinstance(est, float) and est > 0, est
hist = call(6, "mempool.get_fee_histogram", [])
assert isinstance(hist, list), hist
relay = call(7, "blockchain.relayfee", [])
assert isinstance(relay, float) and relay > 0, relay
print("connect fees OK:", est, relay, len(hist))

# Broadcast a real signed transaction through e-lite.
dest = cli("getnewaddress")
dest_sh = sh_of(dest)
utxos = json.loads(cli("listunspent", "100", "9999999"))
u = next(x for x in utxos if x["amount"] >= 0.01)
amount = round(u["amount"] - 0.0001, 8)
raw = cli(
    "createrawtransaction",
    json.dumps([{"txid": u["txid"], "vout": u["vout"]}]),
    json.dumps({dest: amount}),
)
signed = json.loads(cli("signrawtransactionwithwallet", raw))["hex"]
txid = call(8, "blockchain.transaction.broadcast", [signed])
print("broadcast:", txid)

# Verbose and non-verbose transaction lookups, plus block stats.
vtx = call(15, "blockchain.transaction.get", [txid, True])
assert vtx["txid"] == txid, vtx
raw_mempool = call(16, "blockchain.transaction.get", [txid])
assert raw_mempool.startswith("02"), raw_mempool[:8]
stats = call(17, "blockchain.block.stats", [1])
assert "blockhash" in stats, stats
print("verbose tx / block stats OK")

# Wait for the mempool overlay to pick it up.
deadline = time.time() + 20
mem = []
while time.time() < deadline:
    mem = call(9, "blockchain.scripthash.get_mempool", [dest_sh])
    if any(x["tx_hash"] == txid for x in mem):
        break
    time.sleep(1)
else:
    raise SystemExit(f"tx {txid} not in mempool overlay: {mem}")
print("mempool entry OK")

bal = call(10, "blockchain.scripthash.get_balance", [dest_sh])
assert bal["unconfirmed"] > 0, bal
print("unconfirmed balance:", bal)

# Subscribe and mine: expect header and script notifications.
before = call(11, "blockchain.headers.subscribe", [])["height"]
status0 = call(12, "blockchain.scripthash.subscribe", [dest_sh])
cli("generatetoaddress", "1", addr)
s.settimeout(20)
seen_header = seen_script = False
end = time.time() + 25
while time.time() < end and not (seen_header and seen_script):
    line = f.readline()
    if not line:
        break
    msg = json.loads(line)
    if msg.get("method") == "blockchain.headers.subscribe":
        seen_header = msg["params"][0]["height"] == before + 1
    if msg.get("method") == "blockchain.scripthash.subscribe":
        seen_script = msg["params"][0] == dest_sh and msg["params"][1] != status0
assert seen_header, "missing headers notification"
assert seen_script, "missing scripthash notification"
print("notifications OK")

# Indexer catches up and confirms the transaction.
deadline = time.time() + 20
hit = []
while time.time() < deadline:
    hist2 = call(13, "blockchain.scripthash.get_history", [dest_sh])
    hit = [x for x in hist2 if x["tx_hash"] == txid]
    if hit and hit[0]["height"] > 0:
        break
    time.sleep(1)
else:
    raise SystemExit("transaction was not confirmed in the index")
deadline = time.time() + 10
while time.time() < deadline:
    bal2 = call(14, "blockchain.scripthash.get_balance", [dest_sh])
    if bal2["confirmed"] > 0 and bal2["unconfirmed"] == 0:
        break
    time.sleep(1)
assert bal2["confirmed"] > 0 and bal2["unconfirmed"] == 0, bal2
print("confirmed after mine:", hit[0]["height"], bal2)
