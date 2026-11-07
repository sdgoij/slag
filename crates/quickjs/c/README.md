# C headers

The two vendored `quickjs.h` headers define the ABI the C skin implements.
They are selected by feature: `c/include/ng/quickjs.h` for `--features ng`,
`c/include/bellard/quickjs.h` for `--features bellard`.

## Provenance

- `ng/quickjs.h` — QuickJS-NG 0.18 (`quickjs-ng`), vendored byte-for-byte.
- `bellard/quickjs.h` — Bellard QuickJS `2025-09-13`, vendored from
  `https://bellard.org/quickjs/quickjs-2025-09-13.tar.xz` (upstream tarball
  SHA-256 `ad3455cafab56a036e5b94c8e6915e653a09f48e32e72cb66f0d0775047a7bb5`),
  then patched for this facade:
  - `JS_DupValue` / `JS_DupValueRT`'s `return (JSValue)v;` changed to
    `return v;` to satisfy MSVC C2440.
  - the inline `JS_FreeValue` / `JS_DupValue` / `JS_FreeValueRT` /
    `JS_DupValueRT` bodies replaced with declarations, so refcounting routes
    to the facade's exported symbols (the NG model) instead of the Bellard
    `JSRefCountHeader` inline refcount.

## Checksums

The drift gate (`tools/check-quickjs-headers.sh`) pins the *vendored* file,
which for Bellard means the post-patch file, not the upstream tarball:

- `ng/quickjs.h` — `6d3879c856551653a1c64fabe52d6d07dc84e97bac3df45848df626702fc30de`
- `bellard/quickjs.h` — `6904cf9ef0aa0b8ff4e9f32f8fa0e19a3971ebb1433c02a7854c8ea81083ec26`

To re-vendor, update the files and the pinned hashes (and the upstream tarball
hash above if the source version changes) in the same commit.
