all:run
run:
	cargo bootimage
	qemu-system-x86_64 -drive format=raw,file=target/x86_64-simplos/debug/bootimage-simplos.bin
