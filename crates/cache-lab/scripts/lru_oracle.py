#!/usr/bin/env python3
"""Independent, dependency-free LRU correctness oracle for the golden fixture.

Run: python3 scripts/lru_oracle.py fixtures/golden.csv 8192
Does NOT validate Rust; it verifies golden expected counts independently.
"""
import csv
import sys
from collections import OrderedDict


def replay(path: str, capacity: int):
    cached = OrderedDict()
    used = hits = misses = cost = 0
    with open(path, encoding='utf-8', newline='') as file:
        for r in csv.DictReader(file):
            if int(r['speculative']):
                continue
            key = int(r['key'])
            size = int(r['charged_bytes'])
            if key in cached:
                hits += 1
                cached.move_to_end(key)
                continue
            misses += 1
            cost += int(r['refill_cost_ns'])
            if size > capacity:
                continue
            while used + size > capacity:
                _, evicted_size = cached.popitem(last=False)
                used -= evicted_size
            cached[key] = size
            used += size
    return hits, misses, cost


if __name__ == '__main__':
    trace = sys.argv[1] if len(sys.argv) > 1 else 'fixtures/golden.csv'
    capacity = int(sys.argv[2]) if len(sys.argv) > 2 else 8192
    result = replay(trace, capacity)
    print(f'LRU independent oracle at {capacity} bytes: hits={result[0]}, misses={result[1]}, modeled_miss_cost_ns={result[2]}')
    if trace.endswith('golden.csv') and capacity == 8192:
        assert result == (2,3,300), result
