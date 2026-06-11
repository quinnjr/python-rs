# Provenance — CPython 3.0 Vendored Tree

This directory contains a verbatim copy of the standard library and test suite
from CPython 3.0.1, the latest release in the Python 3.0 series, used as the
compatibility target and test oracle for python-rs. See
`docs/superpowers/specs/2026-05-19-cpython-3.0-compatibility-design.md`
for the strategy that motivated this vendoring.

After 3.0.1, the Python project moved directly to the 3.1 line; no further
3.0.x releases were made. 3.0.1 is the final, frozen artifact of the 3.0
series.

## Source

- **Project:** CPython
- **Version:** 3.0.1 (the last release in the 3.0 series)
- **Release date:** 2009-02-13
- **Source URL:** https://www.python.org/ftp/python/3.0.1/Python-3.0.1.tar.bz2
- **Tarball SHA-256:** `91afb6ac16d3d22bc6bfbc80726dc85ede32bf838f660cc67016c7d0a7079add`
- **Fetched on:** 2026-05-19

## What we vendor

| Path | Source | Purpose |
|---|---|---|
| `Lib/` | `Python-3.0/Lib/` (verbatim) | Pure-Python standard library |
| `Lib/test/` | `Python-3.0/Lib/test/` (verbatim) | Regression test suite — our compatibility oracle |
| `LICENSE` | `Python-3.0/LICENSE` (verbatim) | PSF License v2 text |

## What we do NOT vendor

The following directories from the upstream tarball are intentionally omitted
because python-rs reimplements their functionality:

- `Modules/` — CPython's C-implemented modules. Reimplemented in Rust under
  `src/cmodules/`.
- `Python/` — CPython interpreter core (eval loop, frame objects, etc.).
  python-rs has its own.
- `Objects/` — CPython's object model (PyObject, type objects).
  python-rs has its own in `src/object.rs`.
- `Parser/` — CPython's grammar and parser tables. python-rs has its own
  recursive-descent parser in `src/parser.rs`.
- `Include/` — CPython's C headers. Relevant to a future `cpyext`
  compatibility layer, not the 3.0 milestone.
- `Doc/`, `Tools/`, `Demo/`, `Mac/`, `PC/`, `PCbuild/` — documentation,
  developer tooling, platform-specific build scripts.

## License

The vendored tree is distributed under the **Python Software Foundation
License Agreement v2** (see `LICENSE` in this directory). PSF-2.0 is a
permissive license compatible with both MIT and Apache 2.0.

The python-rs project itself (everything outside this `vendor/cpython-3.0/`
directory) remains licensed under **MIT OR Apache-2.0**. The aggregate
distribution is the standard pattern for vendoring third-party code: each
file is governed by its own license, with attribution preserved.

## Modifications

Vendored files are **not edited in place**. Any local modifications live in
`patches/`, one patch file per logical change, applied at build time or
checked in alongside the unmodified original. See `patches/README.md`.

Any patches we ship are derivative works of PSF-licensed code and remain
under PSF-2.0.

## Re-vendoring procedure

To re-derive this tree from upstream:

```sh
curl -L https://www.python.org/ftp/python/3.0.1/Python-3.0.1.tar.bz2 -o /tmp/Python-3.0.1.tar.bz2
# Verify SHA-256:
echo "91afb6ac16d3d22bc6bfbc80726dc85ede32bf838f660cc67016c7d0a7079add  /tmp/Python-3.0.1.tar.bz2" | sha256sum -c
tar -xjf /tmp/Python-3.0.1.tar.bz2 -C /tmp
rm -rf vendor/cpython-3.0/Lib vendor/cpython-3.0/LICENSE
cp -r /tmp/Python-3.0.1/Lib vendor/cpython-3.0/Lib
cp /tmp/Python-3.0.1/LICENSE vendor/cpython-3.0/LICENSE
# Reapply patches by hand if any are present under patches/.
```

The source is frozen; this procedure should produce an identical tree forever.
