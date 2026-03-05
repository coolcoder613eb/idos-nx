diskimage := build/bootdisk.img
userdata := build/datadisk.img
bootsector := build/mbr.bin
bootbin := build/boot.bin
kernel := build/kernel.bin

colordemo := target/i386-idos/release/colordemo
command := target/i386-idos/release/command
diskchk := target/i386-idos/release/diskchk
doslayer := target/i386-idos/release/doslayer
elfload := target/i386-idos/release/elfload
fatdrv_elf := target/i386-idos/release/fatdriver
fatdrv := build/fatdrv.bin
gfx := target/i386-idos/release/gfx
e1000 := target/i386-idos/release/e1000

kernel_build_flags := --release -Zbuild-std=core,alloc -Zbuild-std-features=compiler-builtins-mem --target i386-kernel.json


.PHONY: all clean run runlogs libc

all: bootdisk

clean:
	@rm -r build

run: bootdisk
	@qemu-system-i386 -m 64M -drive format=raw,file=$(diskimage) -serial stdio -fda $(userdata) -device floppy,unit=1,drive= -device isa-debug-exit,iobase=0xf4,iosize=4 -display sdl; \
	EXIT_CODE=$$?; \
	exit $$(($$EXIT_CODE >> 1))

runlogs: bootdisk logview
	@qemu-system-i386 -m 8M -drive format=raw,file=$(diskimage) -serial stdio -fda $(userdata) -device floppy,unit=1,drive= -device isa-debug-exit,iobase=0xf4,iosize=4 -display sdl 2>&1 | target/release/logview; \
	EXIT_CODE=$$?; \
	exit $$(($$EXIT_CODE >> 1))

$(diskimage):
	@mkdir -p $(shell dirname $@)
	@mkfs.msdos -F 12 -C $(diskimage) 16384

$(userdata): $(colordemo)
	@mkdir -p $(shell dirname $@)
	@mkdir -p userdata/disk
	@mkfs.msdos -C $(userdata) 1440
	@cd userdata && make
	@mcopy -D o -i $(userdata) userdata/disk/*.* ::
	@mcopy -D o -i $(userdata) userdata/static/*.* ::
	@mcopy -D o -i $(userdata) $(colordemo) ::COLORS.ELF

bootdisk: $(command) $(diskchk) $(doslayer) $(elfload) $(fatdrv) $(gfx) $(e1000) $(diskimage) $(userdata) $(bootsector) $(bootbin) $(kernel)
	@dd if=$(bootsector) of=$(diskimage) bs=450 count=1 seek=62 skip=62 iflag=skip_bytes oflag=seek_bytes conv=notrunc
	@mcopy -D o -i $(diskimage) $(bootbin) ::BOOT.BIN
	@mcopy -D o -i $(diskimage) $(kernel) ::KERNEL.BIN
	@mcopy -D o -i $(diskimage) $(fatdrv) ::FATDRV.BIN
	@mcopy -D o -i $(diskimage) $(command) ::COMMAND.ELF
	@mcopy -D o -i $(diskimage) $(doslayer) ::DOSLAYER.ELF
	@mcopy -D o -i $(diskimage) $(elfload) ::ELFLOAD.ELF
	@mcopy -D o -i $(diskimage) $(diskchk) ::DISKCHK.ELF
	@mcopy -D o -i $(diskimage) $(gfx) ::GFX.ELF
	@mcopy -D o -i $(diskimage) $(e1000) ::E1000.ELF
	@mcopy -D o -i $(diskimage) resources/ter-i14n.psf ::TERM14.PSF
	@mcopy -D o -i $(diskimage) resources/DRIVERS.CFG ::DRIVERS.CFG

$(bootsector):
	@mkdir -p $(shell dirname $@)
	@cd bootloader/mbr && \
	cargo build --release -Zbuild-std=core -Zbuild-std-features=compiler-builtins-mem --target i386-mbr.json
	@objcopy -I elf32-i386 -O binary target/i386-mbr/release/idos-mbr $(bootsector)

$(bootbin):
	@mkdir -p $(shell dirname $@)
	@cd bootloader/bootbin && \
	cargo build --release -Zbuild-std=core -Zbuild-std-features=compiler-builtins-mem --target i386-bootbin.json
	@objcopy -I elf32-i386 -O binary target/i386-bootbin/release/idos-bootbin $(bootbin)

$(kernel):
	@mkdir -p $(shell dirname $@)
	@cd kernel && \
	cargo build $(kernel_build_flags)
	@cp target/i386-kernel/release/idos_kernel $(kernel)

testkernel:
	@mkdir -p build
	@cd kernel && \
	cargo test --no-run $(kernel_build_flags) &>../testkernel.log
	TEST_EXEC=$(shell grep -Po "Executable unittests src/main.rs \(\K[^\)]+" testkernel.log); \
	cp $$TEST_EXEC $(kernel)

test: testkernel run

$(command):
	@cd components/programs/command && \
	cargo build -Zbuild-std=core,alloc -Zbuild-std-features=compiler-builtins-mem --target ../../i386-idos.json --release

$(doslayer):
	@cd components/programs/doslayer && \
	cargo build -Zbuild-std=core,alloc -Zbuild-std-features=compiler-builtins-mem --target ../../i386-idos.json --release

$(elfload):
	@cd components/programs/elfload && \
	cargo build -Zbuild-std=core,alloc -Zbuild-std-features=compiler-builtins-mem --target ../../i386-idos.json --release

$(colordemo):
	@cd components/programs/colordemo && \
	cargo build -Zbuild-std=core,alloc -Zbuild-std-features=compiler-builtins-mem --target ../../i386-idos.json --release

$(diskchk):
	@cd components/programs/diskchk && \
	cargo build -Zbuild-std=core,alloc -Zbuild-std-features=compiler-builtins-mem --target ../../i386-idos.json --release

$(fatdrv_elf):
	@cd fatdriver && \
	cargo build --features idos -Zbuild-std=core,alloc -Zbuild-std-features=compiler-builtins-mem --target ../components/i386-idos.json --release

$(fatdrv): $(fatdrv_elf)
	@mkdir -p $(shell dirname $@)
	@objcopy -I elf32-i386 -O binary $(fatdrv_elf) $(fatdrv)

$(gfx):
	@cd components/programs/gfx && \
	cargo build -Zbuild-std=core,alloc -Zbuild-std-features=compiler-builtins-mem --target ../../i386-idos.json --release

$(e1000):
	@cd e1000 && \
	cargo build -Zbuild-std=core,alloc -Zbuild-std-features=compiler-builtins-mem --target ../components/i386-idos.json --release

logview:
	cargo build -p logview --release

libc:
	cargo build -p idos-libc --target components/i386-idos.json \
		-Zbuild-std=core,alloc -Zbuild-std-features=compiler-builtins-mem --release
	@cp target/i386-idos/release/libidos_libc.a sysroot/lib/libc.a
	@gcc -m32 -c sysroot/src/crt0.s -o sysroot/lib/crt0.o
