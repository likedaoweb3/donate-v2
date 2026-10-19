#!/usr/bin/env sh
set -eu
cd "$(dirname "$0")/.."
cargo-build-sbf --manifest-path programs/donate/Cargo.toml --sbf-out-dir target/deploy -- --locked
python3 - <<'PY'
from pathlib import Path
import hashlib
p = Path('target/deploy/donate.so')
b = p.read_bytes()
h = hashlib.sha256(b).hexdigest()
assert len(b) == 1534600, (len(b), 'unexpected executable length')
assert h == '91fd6d202135deb006c0b48f9fbd0f1546901393a96d08ba647ad21c9331719d', h
print('Donate build matches the published mainnet executable hash:', h)
PY
