#!/usr/bin/env python3
"""Minimal verified-TLS HTTP/3 smoke client (requires aioquic)."""
import argparse
import asyncio
import sys

from aioquic.asyncio.client import connect
from aioquic.asyncio.protocol import QuicConnectionProtocol
from aioquic.h3.connection import H3_ALPN, H3Connection
from aioquic.h3.events import DataReceived, HeadersReceived
from aioquic.quic.configuration import QuicConfiguration
from aioquic.quic.events import ConnectionTerminated


class Client(QuicConnectionProtocol):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.http = H3Connection(self._quic)
        self.requests = {}

    async def get(self, authority, path):
        stream_id = self._quic.get_next_available_stream_id()
        future = asyncio.get_running_loop().create_future()
        self.requests[stream_id] = (future, None, bytearray())
        self.http.send_headers(
            stream_id=stream_id,
            headers=[
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":authority", authority.encode()),
                (b":path", path.encode()),
            ],
            end_stream=True,
        )
        self.transmit()
        return await asyncio.wait_for(future, timeout=3)

    def quic_event_received(self, event):
        for item in self.http.handle_event(event):
            if not isinstance(item, (HeadersReceived, DataReceived)):
                continue
            entry = self.requests.get(item.stream_id)
            if entry is None:
                continue
            future, status, body = entry
            if isinstance(item, HeadersReceived):
                for key, value in item.headers:
                    if key == b":status":
                        status = int(value)
            else:
                body.extend(item.data)
            self.requests[item.stream_id] = (future, status, body)
            if item.stream_ended:
                self.requests.pop(item.stream_id, None)
                if not future.done():
                    future.set_result((status, bytes(body)))
        if isinstance(event, ConnectionTerminated):
            for future, _, _ in self.requests.values():
                if not future.done():
                    future.set_exception(ConnectionError("HTTP/3 connection terminated"))
            self.requests.clear()


async def run(args):
    configuration = QuicConfiguration(is_client=True, alpn_protocols=H3_ALPN)
    configuration.load_verify_locations(cafile=args.ca_certs)
    configuration.server_name = args.host
    async with connect(
        "127.0.0.1",
        args.port,
        configuration=configuration,
        create_protocol=Client,
    ) as client:
        status, body = await client.get(f"{args.host}:{args.port}", "/")
        if status != 200 or b"Rusty Balancer" not in body:
            raise RuntimeError(f"unexpected HTTP/3 response: {status}, {body[:120]!r}")
        sys.stdout.buffer.write(body + b"\n3\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="localhost")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--ca-certs", required=True)
    arguments = parser.parse_args()
    asyncio.run(asyncio.wait_for(run(arguments), timeout=4))
