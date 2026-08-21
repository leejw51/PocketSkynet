"""Shared fixtures: the canonical protocol test vectors.

The vectors are generated and validated by the Rust suite
(app/core/tests/vectors/protocol-v1.json); a port is correct only when it
reproduces them byte-for-byte.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

VECTORS_PATH = (
    Path(__file__).resolve().parents[3]
    / "core"
    / "tests"
    / "vectors"
    / "protocol-v1.json"
)


@pytest.fixture(scope="session")
def vectors() -> dict:
    return json.loads(VECTORS_PATH.read_text())
