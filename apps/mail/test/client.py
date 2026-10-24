"""An SMTP client for the mail app's tests, on Python's smtplib.

    python3 -I client.py --port N [--starttls] [--tls 1.2|1.3] [--ca FILE]
                         [--timeout S] MESSAGES.json

MESSAGES.json is a list of {"from": str, "to": [str], "data_b64": str,
"mail_options"?: [str]}. Each step's reply is printed as a JSON line:
{"cmd": ..., "code": ..., "msg": ...}; TLS adds {"cmd": "tls", "version",
"cipher"}. Exits 0 when the session ran to QUIT, whatever the replies.
"""

import argparse
import base64
import json
import smtplib
import ssl
import sys


def out(cmd, code, msg=b"", **more):
    if isinstance(msg, bytes):
        msg = msg.decode("utf-8", "replace")
    print(json.dumps({"cmd": cmd, "code": code, "msg": msg, **more}), flush=True)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, required=True)
    p.add_argument("--starttls", action="store_true")
    p.add_argument("--tls", choices=["1.2", "1.3"])
    p.add_argument("--ca")
    p.add_argument("--timeout", type=float, default=30)
    p.add_argument("messages")
    a = p.parse_args()
    with open(a.messages) as f:
        messages = json.load(f)

    s = smtplib.SMTP(timeout=a.timeout)
    code, msg = s.connect(a.host, a.port)
    # starttls() checks the certificate against the host given to the
    # constructor; connect() alone leaves it unset.
    s._host = a.host
    out("connect", code, msg)
    code, msg = s.ehlo("client.test")
    out("ehlo", code, msg)
    if a.starttls:
        ctx = ssl.create_default_context(cafile=a.ca)
        ctx.check_hostname = True
        if a.tls:
            v = ssl.TLSVersion.TLSv1_2 if a.tls == "1.2" else ssl.TLSVersion.TLSv1_3
            ctx.minimum_version = v
            ctx.maximum_version = v
        code, msg = s.starttls(context=ctx)
        out("starttls", code, msg)
        out("tls", 0, version=s.sock.version(), cipher=s.sock.cipher()[0])
        code, msg = s.ehlo("client.test")
        out("ehlo", code, msg)

    for m in messages:
        code, msg = s.mail(m["from"], m.get("mail_options", []))
        out("mail", code, msg)
        if code != 250:
            s.rset()
            continue
        ok = 0
        for to in m["to"]:
            code, msg = s.rcpt(to)
            out("rcpt", code, msg, to=to)
            ok += code == 250
        if not ok:
            s.rset()
            continue
        try:
            code, msg = s.data(base64.b64decode(m["data_b64"]))
        except smtplib.SMTPDataError as e:
            code, msg = e.smtp_code, e.smtp_error
        out("data", code, msg)

    code, msg = s.docmd("QUIT")
    out("quit", code, msg)
    s.close()


if __name__ == "__main__":
    try:
        main()
    except Exception as e:  # noqa: BLE001 - reported to the test as a line
        print(json.dumps({"cmd": "error", "code": -1, "msg": f"{type(e).__name__}: {e}"}), flush=True)
        sys.exit(1)
