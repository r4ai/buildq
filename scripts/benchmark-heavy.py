#!/usr/bin/env python3
"""Measure a command: uv run --project scripts --locked scripts/benchmark-heavy.py."""

from buildq_bench.measurement import main

if __name__ == "__main__":
    raise SystemExit(main())
