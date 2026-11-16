#!/usr/bin/env python3
"""Shared helpers for the e2e suites: a small Electrum client and bitcoin-cli.

Imported by tests/e2e/*.py; also a command for the shell suites:

    e2e.py history <port> <scripthash>                 number of history entries
    e2e.py call [--tls] <port> <method> [json params]  the result, as JSON
    e2e.py height [--tls] <port>                       tip height it serves
"""
import hashlib
import json
import socket
import ssl
import subprocess
import sys


class Electrum:
    """One connection. `call` returns the result and fails on an error;
    `request` returns the whole response. Notifications that arrive in
    between are kept in `notes`."""

    def __init__(self, port, tls=False, cafile=None, timeout=20, host="127.0.0.1"):
        sock = socket.create_connection((host, port), timeout=timeout)
        if tls:
            ctx = ssl.create_default_context(cafile=cafile)
            if cafile is None:
                ctx.check_hostname = False
                ctx.verify_mode = ssl.CERT_NONE
            sock = ctx.wrap_socket(sock, server_hostname="localhost")
        self.sock = sock
        self.f = sock.makefile("rwb")
        self.next_id = 0
        self.notes = []

    def send(self, payload):
        self.f.write(json.dumps(payload).encode() + b"\n")
        self.f.flush()

    def read(self):
        line = self.f.readline()
        if not line:
            raise ConnectionError("server closed the connection")
        return json.loads(line)

    def request(self, method, params=None):
        self.next_id += 1
        rid = self.next_id
        self.send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params or []})
        while True:
            msg = self.read()
            if msg.get("id") == rid:
                return msg
            self.notes.append(msg)

    def call(self, method, params=None):
        msg = self.request(method, params)
        assert "error" not in msg, msg
        return msg["result"]

    def close(self):
        self.sock.close()


def cli_for(core, rpc):
    """`cli(*args)`: bitcoin-cli on a regtest node, its output as text."""

    def cli(*args):
        out = subprocess.run(
            ["bitcoin-cli", "-regtest", f"-datadir={core}", f"-rpcport={rpc}", *map(str, args)],
            capture_output=True,
            text=True,
        )
        if out.returncode != 0:
            raise SystemExit(f"bitcoin-cli {args} failed: {out.stderr.strip()}")
        return out.stdout.strip()

    return cli


def script_hash(script_hex):
    """Electrum script hash of a scriptPubKey."""
    return hashlib.sha256(bytes.fromhex(script_hex)).digest()[::-1].hex()


def main(argv):
    tls = "--tls" in argv
    argv = [a for a in argv if a != "--tls"]
    if len(argv) >= 3 and argv[0] == "history":
        client = Electrum(int(argv[1]), tls=tls)
        client.call("server.version", ["e2e", "1.4"])
        print(len(client.call("blockchain.scripthash.get_history", [argv[2]])))
        return 0
    if len(argv) >= 3 and argv[0] == "call":
        client = Electrum(int(argv[1]), tls=tls)
        params = json.loads(argv[3]) if len(argv) > 3 else []
        print(json.dumps(client.call(argv[2], params)))
        return 0
    if len(argv) == 2 and argv[0] == "height":
        client = Electrum(int(argv[1]), tls=tls)
        print(client.call("blockchain.headers.subscribe")["height"])
        return 0
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
