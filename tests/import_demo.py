# M3 import-system smoke test. Demonstrates every flavor of import the
# interpreter now supports. Runnable via:
#
#     cargo run --release -- tests/import_demo.py
#
# Expected output is in the assertions below (this script does not write
# its own assertion helpers — it just prints and the human / harness
# diffs against expected output).

# 1. cmodule import
import sys
print(sys.version)        # e.g. "3.0.1 (python-rs, compatibility target)"
print(sys.platform)       # linux / darwin / win32
print(sys.maxsize)        # 140737488355327

# 2. from cmodule import name
from sys import maxsize as MS
print(MS)                 # 140737488355327

# 3. from cmodule import *
from sys import *
print(byteorder)          # 'little' or 'big'

# 4. Builtins still work inside any frame (the __builtins__ fallback chain).
print(len("hello"))       # 5
print(abs(-7))            # 7
