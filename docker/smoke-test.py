#!/usr/bin/env python3
"""Checks that a running server answers: Status-Server on both ports, an Access-Reject for a
user that does not exist, and an Accounting-Response for a record with a quote in its values,
which the script in docker/config has to store. Exits non-zero if it has not done so within
the limit."""
import argparse
import hashlib
import hmac
import os
import pathlib
import socket
import sys
import time

ACCESS_REQUEST, ACCESS_ACCEPT, ACCESS_REJECT, ACCOUNTING_REQUEST, ACCOUNTING_RESPONSE, STATUS_SERVER = 1, 2, 3, 4, 5, 12
MESSAGE_AUTHENTICATOR = 80


def signed(code, identifier, attributes, secret):
    """A packet with a Message-Authenticator (RFC 3579), which comes first."""
    authenticator = os.urandom(16)
    body = bytes([MESSAGE_AUTHENTICATOR, 18]) + bytes(16) + attributes
    header = bytes([code, identifier]) + (20 + len(body)).to_bytes(2, "big") + authenticator
    mac = hmac.new(secret, header + body, hashlib.md5).digest()
    return header + bytes([MESSAGE_AUTHENTICATOR, 18]) + mac + attributes, authenticator


def accounting(identifier, attributes, secret):
    """An Accounting-Request, whose authenticator is a hash over the packet (RFC 2866)."""
    header = bytes([ACCOUNTING_REQUEST, identifier]) + (20 + len(attributes)).to_bytes(2, "big")
    authenticator = hashlib.md5(header + bytes(16) + attributes + secret).digest()
    return header + authenticator + attributes, authenticator


def verified_code(reply, authenticator, secret):
    """The code of a reply, after checking its Response Authenticator."""
    expected = hashlib.md5(reply[:4] + authenticator + reply[20:] + secret).digest()
    if reply[4:20] != expected:
        sys.exit("reply with a wrong Response Authenticator: the secret does not match")
    return reply[0]


def exchange(host, port, packet, authenticator, secret):
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.settimeout(1)
        sock.sendto(packet, (host, port))
        try:
            return verified_code(sock.recv(4096), authenticator, secret)
        except (TimeoutError, ConnectionError, OSError):
            return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--auth-port", type=int, default=1812)
    parser.add_argument("--acct-port", type=int, default=1813)
    parser.add_argument("--secret-file", default=pathlib.Path(__file__).parent / "secrets/radius.secret")
    parser.add_argument("--wait", type=int, default=120, help="seconds to wait for the server")
    args = parser.parse_args()
    secret = pathlib.Path(args.secret_file).read_bytes().strip()

    user = b"no-such-user"
    user_attribute = bytes([1, len(user) + 2]) + user
    checks = [
        ("Status-Server, authentication port", args.auth_port, STATUS_SERVER, b"", ACCESS_ACCEPT),
        ("Status-Server, accounting port", args.acct_port, STATUS_SERVER, b"", ACCOUNTING_RESPONSE),
        ("Access-Request for an unknown user", args.auth_port, ACCESS_REQUEST, user_attribute, ACCESS_REJECT),
    ]
    session = b"it's-a-smoke-test"
    session_attribute = bytes([44, len(session) + 2]) + session
    record = bytes([1, 9]) + b"o'brien" + session_attribute + bytes([40, 6, 0, 0, 0, 1])
    checks.append(("Accounting-Request with a quote", args.acct_port, ACCOUNTING_REQUEST, record, ACCOUNTING_RESPONSE))
    deadline = time.monotonic() + args.wait
    for identifier, (name, port, code, attributes, expected) in enumerate(checks):
        while True:
            build = accounting if code == ACCOUNTING_REQUEST else lambda i, a, s: signed(code, i, a, s)
            packet, authenticator = build(identifier, attributes, secret)
            answer = exchange(args.host, port, packet, authenticator, secret)
            if answer == expected:
                print(f"ok: {name}")
                break
            if answer is not None:
                sys.exit(f"{name}: expected code {expected}, got {answer}")
            if time.monotonic() > deadline:
                sys.exit(f"{name}: no answer within {args.wait} s")
            time.sleep(1)


if __name__ == "__main__":
    main()
