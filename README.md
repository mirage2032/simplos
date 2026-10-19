# simplos

A small x86_64 operating system written in Rust, from the boot sector up. It
brings up long mode, a GDT and IDT, paging and a heap, a PS/2 keyboard, the
HPET, and draws a terminal into a linear framebuffer, on which it runs the
first program on its disk, as Linux would, or else a small command line of its
own (type `help`), from which `run` starts unmodified Linux x86-64 programs in
user mode. It boots under both BIOS and UEFI firmware, on real hardware or an
emulator — including [in a browser tab](https://lazyscript.com/projects/simplos), where QEMU itself
is compiled to WebAssembly.

Nothing here is `std`. The kernel is `#![no_std]`, built for
`x86_64-unknown-none` with a rebuilt `core`, and the toolchain is a pinned
nightly (`rust-toolchain.toml`) because `build-std`, artifact dependencies and
`abi_x86_interrupt` are all still unstable.

## Running it

```sh
make run         # UEFI
make run-bios    # BIOS
make debug       # wait for a debugger on :1234 before the first instruction
```

Each is `cargo run` in `tools/boot`, which can be run there just as well
(`cd tools/boot && cargo run -- --bios`), with `ARGS` after its `--`:
`make run ARGS='-m 2G'`.

`tools/boot` is not part of the OS: it is a host program whose build script
turns the kernel into two bootable disk images and whose `main` starts QEMU on
one of them. It uses KVM when `/dev/kvm` is there and falls back to software
emulation when it isn't, so it works on a machine or a CI runner either way.
Any other argument is passed straight through to QEMU. The ext4 disk it
attaches, `tools/boot/disk.img`, is made the first time and kept (see
[Disks and ext4](docs/storage.md)).

A release's images need none of this — download `bios.img` or `uefi.img` from
[the latest release](../../releases/latest) and boot it. Its `disk.qcow2` is
the ext4 disk `cargo run` would make, userland, GCC, Rust and all, 2 GiB to
the guest in a compressed file a fraction of that; attach it after the image with
`-drive format=qcow2,file=disk.qcow2`. To compile on it, attach it to AHCI
instead, as `cargo run` does, which reads four times as fast, and for more
than hello world, give it more memory than QEMU's 128 MiB with `-m 1G`:
`-device ich9-ahci,id=ahci -drive id=d,if=none,format=qcow2,file=disk.qcow2 -device ide-hd,drive=d,bus=ahci.0`.

## Documentation

How each part works, and why, is in `docs/`:

- [Booting](docs/boot.md): the screen's resolution, chosen before the
  kernel runs, and the vendored bootloader.
- [Disks and ext4](docs/storage.md): the IDE and AHCI drivers, the file
  system and what's cached of it, and the vendored lwext4.
- [Processes, signals and time](docs/processes.md): Linux's programs, their
  threads, signals, clocks and timers.
- [Terminal](docs/terminal.md): the line discipline and the screen.
- [What's on the disk](docs/userland.md): the first program, the userland,
  GCC and Rust.
- [Tests](docs/testing.md): how a test kernel boots, and how
  `scripts/mutants.sh` checks the tests notice what they should.
- [Branch rules](docs/branches.md): what develop's and master's rules
  enforce, and why not more.

## Tests

```sh
make test                          # every test kernel, under BIOS and UEFI at once
make test ARGS='--test shells'     # one of them
SIMPLOS_FIRMWARE=bios make test    # under one firmware
make test-ext4                     # the ext4 crate's, on the host
make lint                          # rustfmt and clippy, over every crate
make images smoke                  # the release images, into dist/, booted
```

Every test is a kernel, which `tools/boot-test` boots in QEMU, and the file
systems they leave are judged by e2fsprogs, so both have to be installed.
How it all works is in [Tests](docs/testing.md).

## Layout

| | |
|---|---|
| `kernel/` | the OS. A `no_std` library plus the binary that draws the screen, its source in `arch/` (the GDT, IDT and interrupt controllers), `mm/` (paging, the heap, the page cache), `drivers/` (serial, PS/2, PCI, the disks, the framebuffer, the HPET, fw_cfg), `fs/` (the file tree, /proc, pipes, file locks), `process/` (programs, signals, futexes, ELF, the first program), `tty/` (the line discipline, the console's input, the screen) and `syscall/`; and beside them the scheduler (`thread.rs`), the clocks (`time.rs`) and the kernel's own shell (`shell.rs`). |
| `lib/ext4/` | ext4 (and ext3 and ext2) file systems, read and written with a journal: a Rust interface over the vendored lwext4, tested on the host against e2fsprogs. |
| `user/probe/` | a user program: ordinary Rust with `std`, built for Linux (static musl). Both images and every test boot carry it as the bootloader's ramdisk, and the shell's `run` runs it. |
| `user/rootfs/` | the files copied onto the disk over what's built for it: `/sbin/init`, `/etc` and root's home. |
| `user/ports/` | what builds the rest of it, each into `target/<name>/root`: `userland.sh` busybox, uutils, zsh and vim; `toolchain.sh` GCC; `rust.sh` Rust; and `completions.sh` zsh's record of its completions, once they're all in. |
| `tools/boot/` | host-side: builds the disk images, runs QEMU. |
| `tools/boot-test/` | cargo's test runner for the kernel's bare-metal tests, which cargo starts through its `runner.sh`. |
| `scripts/` | `build-images.sh`, which makes a release's images, `smoke-test.sh`, which boots them, and `mutants.sh`, which checks the tests. |
| `vendor/bootloader/` | [rust-osdev/bootloader](https://github.com/rust-osdev/bootloader) 0.11.17, vendored and patched. |
| `vendor/ps2/` | the `ps2` crate, with its `x86_64` dependency bumped. |
| `vendor/lwext4/` | [lwext4](https://github.com/gkostka/lwext4), the C library `lib/ext4` uses for now. |
| `docs/` | how it all works. |
| `Makefile` | the commands above. |

`tools/` is a cargo workspace: `boot` and `boot-test` share its `Cargo.lock`,
target directory and `.cargo/config.toml`. Nothing else can join it: `kernel`
builds for `x86_64-unknown-none` with `build-std`, `user/probe` for
`x86_64-unknown-linux-musl` (linked by the toolchain's `rust-lld`) and
`lib/ext4`'s tests for the host, and the settings for those are mutually
exclusive. So each of them has its own `Cargo.toml`, `Cargo.lock` and target
directory, and a `[patch]` added to `kernel/Cargo.toml` has to be added to
`tools/Cargo.toml` too, where boot builds the kernel for the images. Cargo
takes `.cargo/config.toml` from the directory it runs in, not the manifest's,
which is why each crate is built from inside its own, and why the root has
none: it would reach the kernel, the builds `user/ports/` runs and the
bootloader's own.

## Branches

`develop` is the default branch and where work lands, through pull requests that
CI checks. `master` is only ever moved by the **Promote to master** workflow,
which checks the commit it is about to release (running again only what no run
has passed with the same sources, usually nothing), bumps the version,
fast-forwards both branches to it, tags it `vX.Y.Z` and publishes a GitHub
Release with both disk images and the ext4 disk (`disk.qcow2`, made by
`scripts/build-images.sh`) attached.

So: branch from `develop`, open a pull request into `develop`, and release by
running Promote to master from the Actions tab.

What the branch rules do and don't enforce, and why: [Branch rules](docs/branches.md).

## License

GPL-2.0-only, the same as Linux, so that Linux code can be ported in where it
saves real work (filesystem drivers, for one). See [LICENSE](LICENSE). The
crates vendored under `vendor/` keep their own licenses (MIT, or MIT/Apache-2.0),
which permit this.
