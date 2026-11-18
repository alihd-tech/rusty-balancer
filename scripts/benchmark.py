#!/usr/bin/env python3
"""Repeatable loopback HTTP/1.1 benchmark; outputs measured JSON, not estimates."""
import argparse
import concurrent.futures
import http.client
import json
import math
import statistics
import time
from urllib.parse import urlsplit

p = argparse.ArgumentParser()
p.add_argument("--url", default="http://127.0.0.1:8080/")
p.add_argument("--requests",type=int,default=10000)
p.add_argument("--concurrency",type=int,default=32)
p.add_argument("--warmup",type=int,default=100)
args = p.parse_args()
url = urlsplit(args.url)
if url.scheme != "http" or url.hostname not in ("127.0.0.1","localhost","::1") or url.username or url.password:
    p.error("benchmark targets must be local HTTP listeners")
if not 1 <= args.requests <= 1_000_000 or not 1 <= args.concurrency <= min(512,args.requests) or not 0 <= args.warmup <= 10000:
    p.error("invalid request/concurrency/warmup counts")
path = url.path or "/"
if url.query:
    path += "?" + url.query
def worker(n):
    conn = http.client.HTTPConnection(url.hostname, url.port or 80, timeout=5)
    samples, failures, received = [], 0, 0
    try:
        for _ in range(n):
            start = time.perf_counter()
            try:
                conn.request("GET",path)
                response=conn.getresponse()
                size=0
                while True:
                    chunk=response.read(16384)
                    if not chunk:
                        break
                    size += len(chunk)
                if not 200 <= response.status < 400:
                    failures += 1
                else:
                    received += size
                    samples.append((time.perf_counter()-start)*1000)
            except (OSError,http.client.HTTPException):
                failures += 1
                conn.close()
    finally:
        conn.close()
    return samples,failures,received

worker(args.warmup)
start=time.perf_counter()
counts=[args.requests//args.concurrency+(i<args.requests%args.concurrency) for i in range(args.concurrency)]
with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) as pool:
    results=list(pool.map(worker,counts))
elapsed=time.perf_counter()-start
samples=sorted(x for s,_,_ in results for x in s)
errors=sum(e for _,e,_ in results)
def percentile(q):
    return round(samples[max(0,math.ceil(len(samples)*q)-1)],3) if samples else None
print(json.dumps({"requests":args.requests,"concurrency":args.concurrency,"seconds":elapsed,
    "successes":len(samples),"errors":errors,"successful_rps":len(samples)/elapsed,
    "received_body_bytes":sum(b for _,_,b in results),
    "latency_ms":{"mean":round(statistics.mean(samples),3) if samples else None,
    "p50":percentile(.5),"p95":percentile(.95),"p99":percentile(.99)}},indent=2))
raise SystemExit(1 if errors else 0)
