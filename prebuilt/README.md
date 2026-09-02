# Prebuilt UF2 images

The three files a badge is flashed with, as built and as run. They are not
built by anything in this repository — they are checked in so that the
transcripts in `badge/logs/` are reproducible against a *specific* binary rather
than against a description of one.

They are gitignored in the working tree (`badge/.gitignore` has `out/`); they are
here deliberately.

## Read this before you flash anything

**These are signed with `xous-core`'s development key. Flashing `loader.uf2`
trips developer mode on the badge, which erases its provisioned secrets — FIDO2
credentials, vault contents, attested boot, badge-to-badge exchange — and
increments a one-way counter. Reflashing stock firmware restores the software and
does *not* restore the secrets or decrement the counter. There is no undo.**

The full reasoning, including why it fails *closed* if you copy only `swap.uf2`
onto a factory badge, is in the repository `README.md` under **Flashing the
badge** and at length in `badge/README.md`.

Save your badge's own stock `loader.uf2`, `xous.uf2` and `swap.uf2` off the badge
before you start. Nothing here redistributes DEF CON's stock firmware.

## The files

| file | sha256 | size |
|---|---|---|
| `loader.uf2` | `8f1a4ba06b68f31ebea762d6a5a51ab130f8f9b2b331f60bc071ba092b86e312` | 354,304 |
| `xous.uf2` | `0c4187fddc42b559d7d2e2d7eea292cde38e297239b37873a7c73ec4347840fd` | 5,629,440 |
| `swap.uf2` | `a56bb424b3ec339562b0fd5621a300730bb0dd341c60f21093b723d12512ed90` | 1,636,352 |

Verify with:

```sh
shasum -a 256 prebuilt/*.uf2      # or: sha256sum
```

## What they were built from

Two different things went into these, and they moved at different times.

**`swap.uf2` — the application.** Built 2026-09-01 22:14 from this project's
source at commit `01cdd4f` ("perf(cache): buy back half the writebacks, and stop
dying on the other half"), packed with `xous-app-uf2 --swap`. `badge/app/` and
`crates/` are **unchanged** between that commit and the tree published here, so
the source in this repository is the source in this image.

This is the image the interactive run used:
`badge/logs/2026-09-02-INTERACTIVE-uname-console.txt`.

**`loader.uf2` and `xous.uf2` — the firmware.** Built 2026-09-01 20:54 from
[`betrusted-io/xous-core`](https://github.com/betrusted-io/xous-core) at
`9844906ddc1214438d0d942d2db2922846ae4722` with `cargo xtask baosec-lite`, with
four of this repository's five patches applied and the fifth deliberately
omitted:

| patch | in these images |
|---|---|
| `usb-bao1x-serialflush-repair.patch` | applied |
| `usb-bao1x-serialrx-repair.patch` | applied |
| `usb-bao1x-drop-in-completion-reset.patch` | applied |
| `xous-log-usb-mirror-nonblocking.patch` | applied |
| `bao1x-hal-usb-in-completion.patch` | **not applied** — it is the writeback regression; see `README.md`, *The firmware patches* |
| `xous-app-uf2-repair.patch` | n/a — a host-tool fix, not firmware |

`badge/*.patch` is unchanged between the commit that produced these
(`3c3524b`) and the tree published here.

### Checking the claims yourself

The swap image carries its build revision. `SwapSourceHeader.partial_nonce`, at
offset 4 of the decoded payload, is the low 16 hex digits of the xous-core commit
the image was packed against:

```sh
xxd -s 36 -l 8 -p swap.uf2      # -> 2db2922846ae4722
```

which is the tail of `9844906ddc1214438d0d942d2db2922846ae4722`. A mismatched
checkout produces an image the badge's loader will not decrypt, so this is not
cosmetic.

That `bao1x-hal-usb-in-completion.patch` is absent is checkable too — the patch's
safety valve is the only string literal it adds, so grep the shipped payload for
it after stripping UF2 block headers:

```python
import struct
raw = open("xous.uf2", "rb").read(); out = bytearray()
for i in range(len(raw) // 512):
    b = raw[i*512:(i+1)*512]
    psize = struct.unpack("<I", b[16:20])[0]
    out += b[32:32+psize]
print(bytes(out).count(b"enqueueing anyway"))   # 0 == clean, 1 == patched
```

It prints `0` for the image here.

## Licensing of these binaries

`loader.uf2` and `xous.uf2` are almost entirely compiled `xous-core`, plus the
four patches above; they carry that project's license terms. `swap.uf2` is this
project's application packed into xous-core's swap-image container. See `LICENSE`
in the repository root.
