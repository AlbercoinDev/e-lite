#!/usr/bin/env python3
"""Electrum 1.7 checks: features, scriptpubkey.*, outpoint.*, testmempoolaccept.

Usage: e2e_electrum17.py <electrum_port> <core_datadir> <rpc_port> <mine_address>
"""
import hashlib
import json
import socket
import subprocess
import sys
import time

port, core, rpc, mine_addr = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]


def cli(*args):
    out = subprocess.run(
        ["bitcoin-cli", "-regtest", f"-datadir={core}", f"-rpcport={rpc}", *args],
        capture_output=True,
        text=True,
    )
    if out.returncode != 0:
        raise SystemExit(f"bitcoin-cli {args} failed: {out.stderr.strip()}")
    return out.stdout.strip()


def script_hash(script_hex):
    return hashlib.sha256(bytes.fromhex(script_hex)).digest()[::-1].hex()


script_hex = json.loads(cli("getaddressinfo", mine_addr))["scriptPubKey"]
sh = script_hash(script_hex)

s = socket.create_connection(("127.0.0.1", port), timeout=20)
f = s.makefile("rwb")
notifications = []


def call(i, method, params):
    f.write(
        (json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params}) + "\n").encode()
    )
    f.flush()
    while True:
        r = json.loads(f.readline())
        if "method" in r and "id" not in r:
            notifications.append(r)
            continue
        assert "error" not in r, (method, r)
        return r["result"]


def wait_for(fn, timeout=25, what="condition"):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = fn()
        if last:
            return last
        time.sleep(0.5)
    raise SystemExit(f"timeout waiting for {what}: {last!r}")


assert call(1, "server.version", ["e2e-1.7", "1.7"])[1] == "1.7"
features = call(2, "server.features", [])
assert features["protocol_max"] == "1.7", features
assert "hash_function" not in features, features

chunk = call(3, "blockchain.block.headers", [0, 2])
assert isinstance(chunk["headers"], list) and len(chunk["headers"]) == 2, chunk
assert "hex" not in chunk, chunk

pong = call(4, "server.ping", [7])
assert isinstance(pong, dict) and len(pong["data"]) == 7, pong

info = call(5, "mempool.get_info", [])
for key in ("mempoolminfee", "minrelaytxfee", "incrementalrelayfee"):
    assert isinstance(info[key], (int, float)), info

# Build a spend from the wallet so testmempoolaccept/outpoint use a real tx.
utxos = json.loads(cli("listunspent", "1", "9999999"))
u = next(x for x in utxos if x["amount"] >= 0.01)
dest = cli("getnewaddress")
amount = round(u["amount"] - 0.0001, 8)
raw = cli(
    "createrawtransaction",
    json.dumps([{"txid": u["txid"], "vout": u["vout"]}]),
    json.dumps({dest: amount}),
)
signed = json.loads(cli("signrawtransactionwithwallet", raw))["hex"]
txid = json.loads(cli("decoderawtransaction", signed))["txid"]

accept = call(6, "blockchain.transaction.testmempoolaccept", [[signed]])
assert accept[0]["allowed"] is True, accept

# Unspent outpoint: UTXO fast path, no external fetch.
status = call(7, "blockchain.outpoint.get_status", [u["txid"], u["vout"], script_hex])
assert "funder_height" in status and "spender_txhash" not in status, status

# Wrong hint must not invent an answer.
mismatch = call(8, "blockchain.outpoint.get_status", [u["txid"], u["vout"], "52"])
assert mismatch == {}, mismatch

subscribed = call(9, "blockchain.outpoint.subscribe", [u["txid"], u["vout"], script_hex])
assert subscribed == status, (subscribed, status)

call(10, "blockchain.transaction.broadcast", [signed])

# Mempool spender becomes visible.
def mempool_spender():
    st = call(11, "blockchain.outpoint.get_status", [u["txid"], u["vout"], script_hex])
    return st if st.get("spender_txhash") == txid else None


mempool_status = wait_for(mempool_spender, what="mempool spender")
assert mempool_status["spender_height"] in (0, -1), mempool_status

recent = call(12, "mempool.recent", [])
assert any(x["txid"] == txid for x in recent), recent

# Mine: outpoint notification and confirmed spender.
call(13, "blockchain.headers.subscribe", [])
cli("generatetoaddress", "1", mine_addr)


def confirmed_spender():
    st = call(14, "blockchain.outpoint.get_status", [u["txid"], u["vout"], script_hex])
    return st if st.get("spender_txhash") == txid and st.get("spender_height", 0) > 0 else None


confirmed = wait_for(confirmed_spender, timeout=40, what="confirmed spender")
assert confirmed["spender_height"] > 0, confirmed
wait_for(
    lambda: any(
        n.get("method") == "blockchain.outpoint.subscribe"
        and n["params"][0] == u["txid"]
        and n["params"][1] == u["vout"]
        for n in notifications
    ),
    what="outpoint notification",
)

# scriptpubkey.* returns the 1.7 dict shapes.
history = call(15, "blockchain.scriptpubkey.get_history", [script_hex])
assert any(x["tx_hash"] == txid for x in history["history"]), history
balance = call(16, "blockchain.scriptpubkey.get_balance", [script_hex])
assert balance["confirmed"] > 0, balance
unspent = call(17, "blockchain.scriptpubkey.listunspent", [script_hex])
assert isinstance(unspent["utxos"], list), unspent
mempool_history = call(18, "blockchain.scriptpubkey.get_mempool", [script_hex])
assert isinstance(mempool_history["history"], list), mempool_history
assert call(19, "blockchain.scriptpubkey.subscribe", [script_hex]) is not None
assert call(20, "blockchain.scriptpubkey.unsubscribe", [script_hex]) is True

# Unknown outpoint and malformed/oversized protocol inputs.
unknown = call(21, "blockchain.outpoint.get_status", ["00" * 32, 0, script_hex])
assert unknown == {}, unknown

print("Electrum 1.7 compatibility OK")
s.close()
