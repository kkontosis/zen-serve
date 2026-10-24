"""An IMAP client for the mail app's tests, on Python's imaplib.

    python3 -I imapclient.py --port N --ca FILE [--timeout S] CALLS.json

CALLS.json is a list of calls on imaplib.IMAP4_SSL, each [method, *args]
(["login", user, password], ["uid", "FETCH", "1:*", "(FLAGS)"]), plus
["response", name] for untagged responses kept by imaplib. Each result is
printed as a JSON line: {"call": method, "typ": ..., "data": [...]}, bytes
as {"b": base64}; a call that raises gives {"call": method, "error": ...}.
"""

import argparse
import base64
import imaplib
import json
import ssl
import sys


def enc(x):
    if isinstance(x, (bytes, bytearray)):
        return {"b": base64.b64encode(bytes(x)).decode()}
    if isinstance(x, (list, tuple)):
        return [enc(y) for y in x]
    return x


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, required=True)
    p.add_argument("--ca", required=True)
    p.add_argument("--timeout", type=float, default=30)
    p.add_argument("calls")
    a = p.parse_args()
    with open(a.calls) as f:
        calls = json.load(f)

    ctx = ssl.create_default_context(cafile=a.ca)
    m = imaplib.IMAP4_SSL(a.host, a.port, ssl_context=ctx, timeout=a.timeout)
    print(json.dumps({"call": "connect", "typ": "OK", "data": [m.welcome.decode()]}), flush=True)
    for c in calls:
        name, args = c[0], c[1:]
        try:
            typ, data = getattr(m, name)(*args)
            print(json.dumps({"call": name, "typ": typ, "data": enc(data)}), flush=True)
        except (imaplib.IMAP4.error, OSError) as e:
            print(json.dumps({"call": name, "error": f"{type(e).__name__}: {e}"}), flush=True)


if __name__ == "__main__":
    try:
        main()
    except Exception as e:  # noqa: BLE001 - reported to the test as a line
        print(json.dumps({"call": "error", "error": f"{type(e).__name__}: {e}"}), flush=True)
        sys.exit(1)
