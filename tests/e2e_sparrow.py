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
        capture_output=True,
        text=True,
    )
    if out.returncode != 0:
        raise SystemExit(f"bitcoin-cli {args} failed: {out.stderr.strip()}")
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
    while True:
        r = json.loads(f.readline())
        # Subscribed clients receive notifications interleaved with responses.
        if "method" in r and "id" not in r:
            continue
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
sender_sh = sh_of(u["address"])
sender_before = call(20, "blockchain.scripthash.get_balance", [sender_sh])
assert sender_before["confirmed"] > 0 and sender_before["unconfirmed"] == 0, sender_before
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

# Confirmed balance ignores mempool events; the delta lives in `unconfirmed`.
sender_after = call(21, "blockchain.scripthash.get_balance", [sender_sh])
assert sender_after["confirmed"] == sender_before["confirmed"], (
    "confirmed balance must ignore the mempool",
    sender_before,
    sender_after,
)
assert sender_after["unconfirmed"] < 0, sender_after
sender_hist = call(22, "blockchain.scripthash.get_history", [sender_sh])
assert any(x["tx_hash"] == txid and x["height"] == 0 for x in sender_hist), sender_hist
print("sender confirmed unchanged, unconfirmed negative")
print("history includes mempool entry")

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

# Verbose lookup after confirmation must carry chain context (blockhash,
# confirmations, time) so wallets do not show the tx as unconfirmed.
vtx_confirmed = call(23, "blockchain.transaction.get", [txid, True])
assert vtx_confirmed["confirmations"] >= 1, vtx_confirmed
assert vtx_confirmed["blockhash"] == cli("getblockhash", str(hit[0]["height"])), vtx_confirmed
assert vtx_confirmed["hex"] == signed, "verbose hex must match the broadcast transaction"
assert vtx_confirmed["time"] > 0, vtx_confirmed
assert vtx_confirmed["blocktime"] == vtx_confirmed["time"], vtx_confirmed
assert vtx_confirmed["in_active_chain"] is True, vtx_confirmed
print("verbose confirmed tx OK")

# --- transaction merkle proof must reach the block's merkle root ---
import hashlib


def sha256d(data):
    return hashlib.sha256(hashlib.sha256(data).digest()).digest()


tx_height = hit[0]["height"]
proof = call(30, "blockchain.transaction.get_merkle", [txid, tx_height])
header_hex = call(31, "blockchain.block.header", [tx_height])
assert isinstance(header_hex, str)
merkle_root = bytes.fromhex(header_hex)[36:68]
node = bytes.fromhex(txid)[::-1]
pos = proof["pos"]
for sibling in proof["merkle"]:
    sib = bytes.fromhex(sibling)[::-1]
    node = sha256d(sib + node) if pos & 1 else sha256d(node + sib)
    pos >>= 1
assert node == merkle_root, "transaction merkle proof does not reach the header root"
print("transaction merkle proof OK at height", tx_height)

# --- checkpoint proof over the whole header chain ---
cp = int(cli("getblockcount"))
leaves = []
for h in range(cp + 1):
    hx = call(40 + h, "blockchain.block.header", [h])
    assert isinstance(hx, str), hx
    leaves.append(sha256d(bytes.fromhex(hx)))


def merkle_root_of(values):
    level = values[:]
    while len(level) > 1:
        if len(level) % 2:
            level.append(level[-1])
        level = [sha256d(level[i] + level[i + 1]) for i in range(0, len(level), 2)]
    return level[0]


expected_root = merkle_root_of(leaves)
single = call(200, "blockchain.block.header", [cp, cp])
assert isinstance(single, dict), single
assert single["header"] == call(201, "blockchain.block.header", [cp])
assert bytes.fromhex(single["root"])[::-1] == expected_root, "checkpoint root mismatch"
node = leaves[cp]
pos = cp
for sibling in single["branch"]:
    sib = bytes.fromhex(sibling)[::-1]
    node = sha256d(sib + node) if pos & 1 else sha256d(node + sib)
    pos >>= 1
assert node == expected_root, "checkpoint branch does not reach the root"
chunk = call(202, "blockchain.block.headers", [0, cp + 1, cp])
assert chunk["count"] == cp + 1, chunk["count"]
assert chunk["root"] == single["root"]
assert chunk["branch"] == single["branch"]
print("checkpoint proof OK at cp_height", cp)

# --- reorg: invalidate the confirming block, then re-mine a longer branch ---
before = tx_height
old_hash = cli("getblockhash", str(before))
cli("invalidateblock", old_hash)
deadline = time.time() + 30
entry = []
while time.time() < deadline:
    hist = call(300, "blockchain.scripthash.get_history", [dest_sh])
    entry = [x for x in hist if x["tx_hash"] == txid]
    bal = call(301, "blockchain.scripthash.get_balance", [dest_sh])
    tip = call(302, "blockchain.headers.subscribe", [])["height"]
    if tip == before - 1 and entry and entry[0]["height"] == 0 and bal["confirmed"] == 0:
        break
    time.sleep(0.5)
else:
    raise SystemExit(f"reorg rollback failed: tip={tip} entry={entry} bal={bal}")
assert bal["unconfirmed"] > 0, bal
print("reorg rollback OK at height", tip)

# Regtest templates are deterministic: a block mined at the same timestamp
# hash-collides with the invalidated one (duplicate-invalid). Move mocktime.
cli("setmocktime", str(int(time.time()) + 500))
cli("generatetoaddress", "2", addr)
cli("setmocktime", "0")
deadline = time.time() + 30
while time.time() < deadline:
    hist = call(303, "blockchain.scripthash.get_history", [dest_sh])
    entry = [x for x in hist if x["tx_hash"] == txid]
    bal = call(304, "blockchain.scripthash.get_balance", [dest_sh])
    if entry and entry[0]["height"] == before and bal["unconfirmed"] == 0 and bal["confirmed"] > 0:
        break
    time.sleep(0.5)
else:
    raise SystemExit(f"reorg reconfirm failed: entry={entry} bal={bal}")
assert cli("getblockhash", str(before)) != old_hash, "the branch must have changed"
print("reorg reconfirm OK at height", entry[0]["height"])
