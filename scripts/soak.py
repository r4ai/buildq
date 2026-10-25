#!/usr/bin/env python3
"""Run the multi-day soak: uv run --project scripts --locked scripts/soak.py --help."""

from buildq_bench.soak import main

if __name__ == "__main__":
    raise SystemExit(main())
