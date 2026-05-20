# Patches Against Vendored CPython 3.0

This directory holds local modifications to the vendored CPython 3.0 tree at
`vendor/cpython-3.0/Lib/`. The vendored files themselves are not edited in
place — every divergence from upstream lives here as a separate patch.

## When a patch is appropriate

- Fixing a known Python 3.0 bug that prevents a test from passing under our
  interpreter, where the upstream fix landed in 3.0.1+ and we want to backport.
- Working around a python-rs limitation we explicitly accept (e.g., a stdlib
  module references a Tier-3 C extension we've stubbed; the patch makes the
  import lazy).
- Adjusting hard-coded `sys.version` checks that incorrectly gate on CPython
  internals python-rs does not share.

## When a patch is NOT appropriate

- "Make this test pass without fixing the underlying interpreter bug." Fix
  the interpreter.
- Style or cleanup changes to upstream files.
- Anything that drifts the vendored tree further from CPython 3.0's actual
  behavior. The point of vendoring is to use *their* code as the oracle.

## Format

Each patch is a standard unified diff named `NNN-short-description.patch`,
where `NNN` is a zero-padded sequence number for ordering. The patch
**must begin with a comment block** explaining:

1. What it changes (one line)
2. Why (the underlying reason — a CPython bug, a python-rs constraint, etc.)
3. References (CPython issue tracker URL, our spec section, etc.)
4. License note: the patch is a derivative work of PSF-licensed code and is
   distributed under the PSF License v2.

Example:

```
# 001-fix-test_xyz-collection-order.patch
#
# What: Adjusts test_xyz to not depend on dict insertion order.
# Why:  CPython 3.0 dicts are unordered; the test was relying on an
#       accidental order. Same fix landed upstream in 3.1 (CPython issue #1234).
# Refs: docs/superpowers/specs/2026-05-19-cpython-3.0-compatibility-design.md M9
# License: PSF-2.0 (derivative of PSF-licensed code)
--- a/Lib/test/test_xyz.py
+++ b/Lib/test/test_xyz.py
@@ ...
```

## Applying patches

Patches are kept in sequence-numbered order. To apply:

```sh
cd vendor/cpython-3.0
for p in patches/*.patch; do
    patch -p1 < "$p"
done
```

(Or apply at build time via a small script — exact mechanism TBD when the
first patch is actually needed.)

## Current patches

None. This directory is empty by design until a real need surfaces.
