#!/usr/bin/env python3
"""Query history for a scripthash and print the number of entries."""
import json
import socket
import sys

port = int(sys.argv[1])
script_hash = sys.argv[2]
s = socket.create_connection(("127.0.0.1", port), timeout=10)
f = s.makefile("rwb")


def call(i, method, params):
    f.write((json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params}) + "\n").encode())
    f.flush()
    r = json.loads(f.readline())
    assert "error" not in r, r
    return r["result"]


call(1, "server.version", ["check", "1.4"])
print(len(call(2, "blockchain.scripthash.get_history", [script_hash])))
