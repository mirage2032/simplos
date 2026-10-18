# Booting

## Resolution

Both images come up at **1920x1080** and the kernel will narrow that on request:

```sh
cargo run -- -fw_cfg name=opt/simplos/fb,string=1024x768
```

Which is worth explaining, because the two firmware paths get there very
differently. The mode is chosen *before* the kernel exists — UEFI through the
GOP, BIOS through a VESA call in the bootloader's 16-bit real-mode second stage —
and neither can be told anything at runtime. So the built-in mode is a build-time
constant, `SIMPLOS_FB_WIDTH` / `SIMPLOS_FB_HEIGHT`, read by both `kernel/build.rs`
and the vendored bootloader's `bios/stage-2/build.rs` so the UEFI minimum and the
BIOS cap cannot drift apart.

The runtime request is a different mechanism: the kernel reads
`opt/simplos/fb` from QEMU's fw_cfg device (`kernel/src/drivers/fwcfg.rs`) and sets
the mode itself through the Bochs VBE registers (`kernel/src/drivers/video/vbe.rs`).

**It can only make the framebuffer smaller.** The bootloader maps exactly
`FrameBufferInfo::byte_len` bytes of video memory and nothing later maps more, so
a larger mode would write past the end of the mapping. A request that doesn't
fit is refused and the firmware's mode kept — which is also what happens on real
hardware, where those registers don't exist.

One 1920x1080 image therefore serves everyone, the browser build included.

## The vendored bootloader

`vendor/bootloader` is upstream 0.11.17 with three local changes, each marked
`LOCAL PATCH` in the source:

- **`bios/stage-2/build.rs`** and **`bios/stage-2/src/main.rs`** — the VESA mode
  cap comes from `SIMPLOS_FB_WIDTH` / `SIMPLOS_FB_HEIGHT` instead of being
  hardcoded.
- **`build.rs`** — declares those two variables as build inputs. Without it the
  nested `cargo install` runs that build the BIOS stages are never re-invoked
  when they change, and a stale stage-2 is silently reused with the old cap
  compiled in.
- **`uefi/src/main.rs`** — prefers an exact GOP mode match before falling back
  to upstream's "first mode at least this large".

Its build is unusual and worth knowing about before debugging it: `build.rs`
shells out to five nested `cargo install --locked` runs to build four real-mode
BIOS stages and the UEFI loader, each for its own custom target with its own
rebuilt `core`. They go to `vendor/bootloader/target`, a target directory of its own.
Because they are `--locked`, editing a vendored manifest without updating
`vendor/bootloader/Cargo.lock` fails *inside a build script*, where the error is
easy to misread.
