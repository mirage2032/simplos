# The front door: what's done most, from the repository's root.
#
#   make run [ARGS=...]     boot the UEFI image in QEMU, ARGS passed to it
#   make run-bios           the same, the BIOS image
#   make debug              the same, waiting for a debugger on :1234
#   make test [ARGS=...]    the kernel's tests, booted (ARGS='--test shells')
#   make test-ext4          the ext4 crate's tests, on the host
#   make images             release images into dist/ (ARGS='--profile dev')
#   make smoke              boot dist/'s images and check what they say
#   make lint               rustfmt and clippy over every crate
#   make fmt                rustfmt every crate
#
# Each runs cargo from inside the crate, never with --manifest-path: cargo
# takes .cargo/config.toml from the directory it runs in, not the manifest's,
# and the crates' settings exclude each other (see README's Layout).

SHELL := bash
.SHELLFLAGS := -euo pipefail -c
.DEFAULT_GOAL := help

# Every crate of ours, vendor/'s being upstream code with its own style.
CRATES := kernel fullkernel tools/boot-test user/probe lib/ext4

# GitHub folds each crate's output under its name; elsewhere it's a heading.
ifdef GITHUB_ACTIONS
group := echo "::group::$$crate"
endgroup := echo "::endgroup::"
else
group := echo "== $$crate"
endgroup := :
endif

.PHONY: help run run-bios debug test test-ext4 images smoke lint fmt fmt-check clippy

help:
	@sed -n 's/^#   //p' Makefile

run:
	cd fullkernel && cargo run -- $(ARGS)

run-bios:
	cd fullkernel && cargo run -- --bios $(ARGS)

debug:
	cd fullkernel && cargo run -- --debug $(ARGS)

test:
	cd kernel && cargo test $(ARGS)

test-ext4:
	cd lib/ext4 && cargo test $(ARGS)

images:
	scripts/build-images.sh $(ARGS)

smoke:
	scripts/smoke-test.sh $(ARGS)

lint: fmt-check clippy

fmt:
	for crate in $(CRATES); do (cd "$$crate" && cargo fmt); done

fmt-check:
	for crate in $(CRATES); do $(group); (cd "$$crate" && cargo fmt --check); $(endgroup); done

# `-D warnings` reaches only the crate being linted, so the vendored
# bootloader and ps2 are checked but not gated — which is what we want,
# and worth knowing before someone goes looking for why.
clippy:
	for crate in $(CRATES); do $(group); (cd "$$crate" && cargo clippy --all-targets -- -D warnings); $(endgroup); done
