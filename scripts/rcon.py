#!/usr/bin/env python3
"""Send an rcon command to the test server and print the reply.

    python scripts/rcon.py "status"
    python scripts/rcon.py "mp_friendlyfire"

This lives in the repo rather than a scratch directory on purpose: it is the
only channel that tells the truth about the running server. The log file lags
in 8 KB blocks and `docker logs` is cumulative across restarts, so both can show
you a world that stopped existing minutes ago. `rcon status` cannot.

Reading a cvar out of cfg/server.cfg proves nothing either -- a config that
never got mounted and one that did look identical from the host. Ask the server.
"""
import re
import socket
import sys

ADDR = ("127.0.0.1", 27015)
PASSWORD = "aiplayers_local"
TIMEOUT = 3.0


def rcon(command, addr=ADDR, password=PASSWORD):
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(TIMEOUT)
    try:
        # The challenge is single-use and per-address.
        sock.sendto(b"\xff\xff\xff\xffchallenge rcon\n", addr)
        reply = sock.recv(4096).decode("latin-1", "replace")
        # The reply is `challenge rcon <n>\n\0`, and that trailing NUL is its
        # own token to str.split() -- taking [-1] yields "\0", which strips to
        # the empty string and the server answers "No challenge for your
        # address", a message that sounds like a firewall problem.
        match = re.search(r"challenge rcon (-?\d+)", reply)
        if not match:
            raise RuntimeError(f"no challenge in {reply!r}")
        challenge = match.group(1)

        msg = f'\xff\xff\xff\xffrcon {challenge} "{password}" {command}\n'
        sock.sendto(msg.encode("latin-1"), addr)

        # A long reply (`status` with 20 players) arrives as several datagrams.
        out = []
        while True:
            try:
                out.append(sock.recv(8192).decode("latin-1", "replace"))
            except socket.timeout:
                break
        return "".join(out)
    finally:
        sock.close()


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(2)
    text = rcon(" ".join(sys.argv[1:]))
    # Strip the connectionless header and the print marker.
    for line in text.replace("\xff\xff\xff\xffl", "").split("\n"):
        line = line.strip("\x00").rstrip()
        if line:
            print(line)
