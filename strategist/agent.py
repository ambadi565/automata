"""Entry point: `uv run --project strategist strategist/agent.py premarket|review|hourly|eod`."""

import sys

from saarthi_strategist.cli import main

if __name__ == "__main__":
    sys.exit(main())
