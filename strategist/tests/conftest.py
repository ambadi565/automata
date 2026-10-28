import json
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[2]


@pytest.fixture
def schema() -> dict:
    return json.loads((REPO / "config" / "plan.schema.json").read_text())
