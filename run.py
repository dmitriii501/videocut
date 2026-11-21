#!/usr/bin/env python3
"""Entrypoint: python -m streamreels ... / ./run.py ..."""
import sys

from streamreels.cli import main

if __name__ == "__main__":
    sys.exit(main())
