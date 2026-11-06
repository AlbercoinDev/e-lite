#!/usr/bin/env python3
"""Electrum 1.6 checks: headers array, mempool info, fee mode and package broadcast.

Usage: e2e_electrum16.py <electrum_port> <core_datadir> <rpc_port> <mine_address>
"""
import json
import socket
import subprocess
import sys
import time

port, core, rpc, _addr = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]


def cli(*args):
    out = subprocess.run(
        ["bitcoin-cli", "-regtest", f"-datadir={core}", f"-rpcport={rpc}", *args],
        capture_output=True,
        text=True,
    )
    if out.returncode != 0:
        raise SystemExit(f"bitcoin-cli {args} failed: {out.stderr.strip()}")
    return out.stdout.strip()


def connect(protocol):
    s = socket.create_connection(("127.0.0.1", port), timeout=20)
    f = s.makefile("rwb")

    def call(i, method, params):
        f.write(
            (json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params}) + "\n").encode()
        )
        f.flush()
        while True:
            r = json.loads(f.readline())
            if "method" in r and "id" not in r:
                continue
            return r

    version = call(0, "server.version", ["e2e-1.6", protocol])
    assert "error" not in version, version
    return s, f, call, version["result"][1]


# --- 1.6 connection ---
s, f, call, negotiated = connect("1.6")
assert negotiated == "1.6", negotiated
features = call(1, "server.features", [])["result"]
assert features["protocol_max"] == "1.6", features
assert features["hash_function"] == "sha256", features

chunk = call(2, "blockchain.block.headers", [0, 2])["result"]
assert isinstance(chunk["headers"], list), chunk
assert len(chunk["headers"]) == 2, chunk
assert "hex" not in chunk, chunk

info = call(3, "mempool.get_info", [])["result"]
for key in ("mempoolminfee", "minrelaytxfee", "incrementalrelayfee"):
    assert isinstance(info[key], (int, float)), info
fee = call(4, "blockchain.estimatefee", [2, "economical"])["result"]
assert isinstance(fee, (int, float)) and fee > 0, fee
assert call(5, "blockchain.estimatefee", [2, "bogus"])["error"]["code"] == -32602

# --- package broadcast: parent pays a wallet address, child spends it ---
utxos = json.loads(cli("listunspent", "1", "9999999"))
u = next(x for x in utxos if x["amount"] >= 0.02)
parent_dest = cli("getnewaddress")
child_dest = cli("getnewaddress")
parent_amount = round(u["amount"] - 0.0001, 8)

raw_parent = cli(
    "createrawtransaction",
    json.dumps([{"txid": u["txid"], "vout": u["vout"]}]),
    json.dumps({parent_dest: parent_amount}),
)
signed_parent = json.loads(cli("signrawtransactionwithwallet", raw_parent))["hex"]
parent = json.loads(cli("decoderawtransaction", signed_parent))
parent_txid = parent["txid"]
parent_spk = parent["vout"][0]["scriptPubKey"]["hex"]

child_amount = round(parent_amount - 0.0001, 8)
raw_child = cli(
    "createrawtransaction",
    json.dumps([{"txid": parent_txid, "vout": 0}]),
    json.dumps({child_dest: child_amount}),
)
prevtxs = json.dumps(
    [
        {
            "txid": parent_txid,
            "vout": 0,
            "scriptPubKey": parent_spk,
            "amount": parent_amount,
        }
    ]
)
signed_child = json.loads(
    cli("signrawtransactionwithwallet", raw_child, prevtxs)
)["hex"]
child_txid = json.loads(cli("decoderawtransaction", signed_child))["txid"]

result = call(6, "blockchain.transaction.broadcast_package", [[signed_parent, signed_child]])
assert "error" not in result, result
assert result["result"]["success"] is True, result
mempool = json.loads(cli("getrawmempool"))
assert parent_txid in mempool and child_txid in mempool, (parent_txid, child_txid, mempool)
print("package broadcast OK:", parent_txid, child_txid)

# A malformed package must be a JSON-RPC error, not a crash.
bad = call(7, "blockchain.transaction.broadcast_package", [["00"]])
assert "error" in bad, bad

# --- 1.4 clients keep the old shape on the same server ---
s14, f14, call14, negotiated14 = connect("1.4")
assert negotiated14 == "1.4", negotiated14
old = call14(1, "blockchain.block.headers", [0, 2])["result"]
assert isinstance(old["hex"], str), old
assert "headers" not in old, old
s14.close()
s.close()
print("Electrum 1.6 compatibility OK")
