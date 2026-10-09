#!/usr/bin/env python3
"""Minimal SIP UAS for the spike: listens on UDP, answers INVITE with 200 OK + SDP,
acks BYE. Not RFC-complete — just enough to prove drachtio originates an INVITE
under our Rust router's control and completes a dialog. Prints what it receives."""
import socket, sys, re

BIND = ("127.0.0.1", 5080)
# A minimal answer SDP (PCMU) pointing at a dummy media port on loopback.
ANSWER_SDP = (
    "v=0\r\n"
    "o=uas 1 1 IN IP4 127.0.0.1\r\n"
    "s=spike-uas\r\n"
    "c=IN IP4 127.0.0.1\r\n"
    "t=0 0\r\n"
    "m=audio 41000 RTP/AVP 0\r\n"
    "a=rtpmap:0 PCMU/8000\r\n"
)

def header(msg, name):
    m = re.search(rf"^{name}:\s*(.*)$", msg, re.IGNORECASE | re.MULTILINE)
    return m.group(1).strip() if m else ""

def main():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind(BIND)
    print(f"UAS listening on {BIND[0]}:{BIND[1]}", flush=True)
    while True:
        data, addr = s.recvfrom(65535)
        msg = data.decode("utf-8", "replace")
        first = msg.split("\r\n", 1)[0]
        print(f"<<< from {addr}: {first}", flush=True)
        if msg.startswith("INVITE"):
            via = header(msg, "Via"); frm = header(msg, "From")
            to = header(msg, "To"); callid = header(msg, "Call-ID")
            cseq = header(msg, "CSeq")
            to_tagged = to if ";tag=" in to else to + ";tag=uas-tag-1"
            resp = (
                "SIP/2.0 200 OK\r\n"
                f"Via: {via}\r\n"
                f"From: {frm}\r\n"
                f"To: {to_tagged}\r\n"
                f"Call-ID: {callid}\r\n"
                f"CSeq: {cseq}\r\n"
                "Contact: <sip:uas@127.0.0.1:5080>\r\n"
                "Content-Type: application/sdp\r\n"
                f"Content-Length: {len(ANSWER_SDP)}\r\n"
                "\r\n"
                f"{ANSWER_SDP}"
            )
            s.sendto(resp.encode(), addr)
            print(f">>> 200 OK (Call-ID {callid})", flush=True)
        elif msg.startswith("BYE"):
            via = header(msg, "Via"); frm = header(msg, "From")
            to = header(msg, "To"); callid = header(msg, "Call-ID"); cseq = header(msg, "CSeq")
            resp = (f"SIP/2.0 200 OK\r\nVia: {via}\r\nFrom: {frm}\r\nTo: {to}\r\n"
                    f"Call-ID: {callid}\r\nCSeq: {cseq}\r\nContent-Length: 0\r\n\r\n")
            s.sendto(resp.encode(), addr)
            print(f">>> 200 OK (BYE {callid})", flush=True)

if __name__ == "__main__":
    main()
