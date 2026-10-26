"""Exercise staged cargo run with a real ripgrep binary in three worktrees."""

import argparse
from pathlib import Path
import os
import json
from buildq_bench.measurement import measure

p = argparse.ArgumentParser()
p.add_argument("--root", type=Path, required=True)
p.add_argument("--buildq", required=True)
a = p.parse_args()
root = a.root.resolve()
input = root / "search-input.txt"
input.write_text(
    "".join(
        ("needle match\n" if i % 10 == 0 else "ordinary text\n") for i in range(100000)
    )
)
env = {
    **os.environ,
    "BUILDQ_DIR": str(root / "state"),
    "BUILDQ_CACHE": str(root / "cache"),
    "BUILDQ_CONFIG": str(root / "state/config.toml"),
    "CARGO_HOME": str(root / "cargo-home"),
    "CARGO_BUILD_JOBS": "1",
    "TMPDIR": str(root / "tmp"),
    "RUSTC_WRAPPER": "",
    "BUILDQ_LABEL": "qa-ripgrep-run",
}
results = []
for i in range(3):
    r = measure(
        f"ripgrep-run-{i}",
        root / "worktrees" / f"ripgrep-{i}",
        root / "logs" / "run",
        [a.buildq, "cargo", "run", "-p", "ripgrep", "--", "-c", "^needle", str(input)],
        env,
    )
    out = (root / "logs" / "run" / f"ripgrep-run-{i}.stdout").read_text().strip()
    results.append(
        {
            "worktree": i,
            "exit": r.exit,
            "stdout": out,
            "correct": out == "10000",
            "wall_s": r.wall_s,
        }
    )
(root / "logs" / "run" / "correctness.json").write_text(
    json.dumps(results, indent=2) + "\n"
)
