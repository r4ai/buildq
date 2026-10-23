#!/usr/bin/env python3
"""Run Bevy scenarios: uv run --project scripts --locked scripts/stress-bevy.py."""

from buildq_bench.scenarios import main

if __name__ == "__main__":
    raise SystemExit(main())
