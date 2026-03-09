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
floppy := target/i386-idos/release/floppy
sb16 := target/i386-idos/release/sb16
netcat := target/i386-idos/release/netcat
gopher := target/i386-idos/release/gopher
tonegen := target/i386-idos/release/tonegen

kernel_build_flags := --release -Zbuild-std=core,alloc -Zbuild-std-features=compiler-builtins-mem --target i386-kernel.json
idos_build_flags := -Zbuild-std=core,alloc -Zbuild-std-features=compiler-builtins-mem --target ../../i386-idos.json --release

# Per-crate source tracking — shared libs that most crates depend on
src_api := $(shell find api/src -name '*.rs') api/Cargo.toml
src_sdk := $(shell find sdk/src -name '*.rs') sdk/Cargo.toml
src_shared := $(src_api) $(src_sdk)

src_kernel := $(shell find kernel/src -name '*.rs') kernel/Cargo.toml $(src_api)
src_bootmbr := $(shell find bootloader/mbr/src -name '*.rs') bootloader/mbr/Cargo.toml
src_bootbin := $(shell find bootloader/bootbin/src -name '*.rs') bootloader/bootbin/Cargo.toml

src_command := $(shell find components/programs/command/src -name '*.rs') components/programs/command/Cargo.toml $(src_shared)
src_doslayer := $(shell find components/programs/doslayer/src -name '*.rs') components/programs/doslayer/Cargo.toml $(src_shared)
src_elfload := $(shell find components/programs/elfload/src -name '*.rs') components/programs/elfload/Cargo.toml $(src_api)
src_colordemo := $(shell find components/programs/colordemo/src -name '*.rs') components/programs/colordemo/Cargo.toml $(src_shared)
src_diskchk := $(shell find components/programs/diskchk/src -name '*.rs') components/programs/diskchk/Cargo.toml $(src_shared)
src_gfx := $(shell find components/programs/gfx/src -name '*.rs') components/programs/gfx/Cargo.toml $(src_shared)
src_tonegen := $(shell find components/programs/tonegen/src -name '*.rs') components/programs/tonegen/Cargo.toml $(src_shared)
src_netcat := $(shell find components/programs/netcat/src -name '*.rs') components/programs/netcat/Cargo.toml $(src_shared)
src_gopher := $(shell find components/programs/gopher/src -name '*.rs') components/programs/gopher/Cargo.toml $(src_shared)
src_fatdrv := $(shell find components/drivers/fatdriver/src -name '*.rs') components/drivers/fatdriver/Cargo.toml $(src_api)
src_e1000 := $(shell find components/drivers/e1000/src -name '*.rs') components/drivers/e1000/Cargo.toml $(src_shared)
src_floppy := $(shell find components/drivers/floppy/src -name '*.rs') components/drivers/floppy/Cargo.toml $(src_shared)
src_sb16 := $(shell find components/drivers/sb16/src -name '*.rs') components/drivers/sb16/Cargo.toml $(src_shared)

qemu_flags := -m 64M -drive format=raw,file=$(diskimage) -serial stdio \
	-fda $(userdata) -device floppy,unit=1,drive= \
	-device isa-debug-exit,iobase=0xf4,iosize=4 \
	-audiodev sdl,id=snd0 -device sb16,audiodev=snd0,irq=5 \
	-display sdl

.PHONY: all clean run runlogs libc

all: bootdisk

clean:
	@rm -r build

run: bootdisk
	@SDL_VIDEO_DRIVER=x11 qemu-system-i386 $(qemu_flags); \
	EXIT_CODE=$$?; \
	exit $$(($$EXIT_CODE >> 1))

runlogs: bootdisk logview
	@SDL_VIDEO_DRIVER=x11 qemu-system-i386 $(qemu_flags) 2>&1 | target/release/logview; \
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

bootdisk: $(command) $(diskchk) $(doslayer) $(elfload) $(fatdrv) $(gfx) $(e1000) $(floppy) $(sb16) $(netcat) $(gopher) $(tonegen) $(diskimage) $(userdata) $(bootsector) $(bootbin) $(kernel)
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
	@mcopy -D o -i $(diskimage) $(floppy) ::FLOPPY.ELF
	@mcopy -D o -i $(diskimage) $(sb16) ::SB16.ELF
	@mcopy -D o -i $(diskimage) $(netcat) ::NETCAT.ELF
	@mcopy -D o -i $(diskimage) $(gopher) ::GOPHER.ELF
	@mcopy -D o -i $(diskimage) $(tonegen) ::TONEGEN.ELF
	@mcopy -D o -i $(diskimage) resources/ter-i14n.psf ::TERM14.PSF
	@mcopy -D o -i $(diskimage) resources/DRIVERS.CFG ::DRIVERS.CFG

$(bootsector): $(src_bootmbr)
	@mkdir -p $(shell dirname $@)
	@cd bootloader/mbr && \
	cargo build --release -Zbuild-std=core -Zbuild-std-features=compiler-builtins-mem --target i386-mbr.json
	@objcopy -I elf32-i386 -O binary target/i386-mbr/release/idos-mbr $(bootsector)

$(bootbin): $(src_bootbin)
	@mkdir -p $(shell dirname $@)
	@cd bootloader/bootbin && \
	cargo build --release -Zbuild-std=core -Zbuild-std-features=compiler-builtins-mem --target i386-bootbin.json
	@objcopy -I elf32-i386 -O binary target/i386-bootbin/release/idos-bootbin $(bootbin)

$(kernel): $(src_kernel)
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

$(command): $(src_command)
	@cd components/programs/command && \
	cargo build $(idos_build_flags)

$(doslayer): $(src_doslayer)
	@cd components/programs/doslayer && \
	cargo build $(idos_build_flags)

$(elfload): $(src_elfload)
	@cd components/programs/elfload && \
	cargo build $(idos_build_flags)

$(colordemo): $(src_colordemo)
	@cd components/programs/colordemo && \
	cargo build $(idos_build_flags)

$(diskchk): $(src_diskchk)
	@cd components/programs/diskchk && \
	cargo build $(idos_build_flags)

$(fatdrv_elf): $(src_fatdrv)
	@cd components/drivers/fatdriver && \
	cargo build --features idos $(idos_build_flags)

$(fatdrv): $(fatdrv_elf)
	@mkdir -p $(shell dirname $@)
	@objcopy -I elf32-i386 -O binary $(fatdrv_elf) $(fatdrv)

$(gfx): $(src_gfx)
	@cd components/programs/gfx && \
	cargo build $(idos_build_flags)

$(e1000): $(src_e1000)
	@cd components/drivers/e1000 && \
	cargo build $(idos_build_flags)

$(floppy): $(src_floppy)
	@cd components/drivers/floppy && \
	cargo build $(idos_build_flags)

$(sb16): $(src_sb16)
	@cd components/drivers/sb16 && \
	cargo build $(idos_build_flags)

$(tonegen): $(src_tonegen)
	@cd components/programs/tonegen && \
	cargo build $(idos_build_flags)

$(netcat): $(src_netcat)
	@cd components/programs/netcat && \
	cargo build $(idos_build_flags)

$(gopher): $(src_gopher)
	@cd components/programs/gopher && \
	cargo build $(idos_build_flags)

logview:
	cargo build -p logview --release

libc:
	cargo build -p idos-libc --target components/i386-idos.json \
		-Zbuild-std=core,alloc -Zbuild-std-features=compiler-builtins-mem --release
	@cp target/i386-idos/release/libidos_libc.a sysroot/lib/libc.a
	@gcc -m32 -c sysroot/src/crt0.s -o sysroot/lib/crt0.o
