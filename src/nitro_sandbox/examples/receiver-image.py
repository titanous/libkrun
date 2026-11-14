#!/usr/bin/env python3
"""Compatibility command; the supported injector lives with the library assets."""
from pathlib import Path
import runpy

runpy.run_path(str(Path(__file__).resolve().parents[1] / 'src' / 'receiver_image' / 'receiver-image.py'), run_name='__main__')
