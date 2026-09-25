# ParanukOS

[English] | [中文](README_CN.md)

ParanukOS is a high-reliability, Rust-based microkernel operating system designed for modern hardware and secure application execution. By leveraging a microkernel architecture and a user-state focused service model, ParanukOS aims to build a robust platform that supports both low-level systems programming and high-level Web/Wasm applications.

## 🔗 Reference Project
This project is inspired by and references the architecture of [os.phil-opp.com](https://os.phil-opp.com/).

## 🌟 Vision & Philosophy
ParanukOS is built on the principle of **Strict Isolation**. We believe that system stability is achieved by minimizing the kernel's responsibilities and isolating every component—from drivers to file systems—into independent user-state services.

*   **Microkernel Core:** Only handles IPC, memory management, and basic scheduling.
*   **High Reliability:** A crash in a driver or file system service does not bring down the entire OS.
*   **Modern Hardware Focus (explicitly bounded):** x86-64 + UEFI + ACPI 6.x only — no BIOS/CSM, no 32-bit x86, no legacy device support.
*   **Wasm-First Application Layer:** Provides a secure, sandboxed environment for users to run software via WebAssembly.

## 🚀 Key Features

### 🏗️ Microkernel Architecture
A pure Rust implementation of a microkernel that provides:
*   **Zero-copy IPC:** High-performance communication between system services.
*   **Capability-based Security:** Fine-grained permission management for all hardware resources.
*   **SMP Support:** Native multi-core scheduling and synchronization.

### 💾 File System Strategy
*   **Now:** FAT32 only — the ESP already is FAT32, so no new driver is needed to read the volume the image was loaded from.
*   **Later (needs its own interface document):** A pure Rust, user-state **Copy-on-Write (CoW)** file system inspired by TFS, providing physical-level isolation of data writes.
*   **Explicitly not short-term: read-only Ext4** — extents, the journal, indirect blocks and checksums make it a multi-month effort with almost no payoff during bring-up.

### 🖥️ Hardware & Booting
*   **UEFI Bootloader (no GRUB):** A UEFI application built on [uefi-rs](https://github.com/rust-osdev/uefi-rs) 0.39 and compiled for `x86_64-unknown-uefi`. It locates the ESP it was started from, validates the kernel image as an ELF64/x86-64 executable and loads it into memory. GRUB is not used anywhere in the boot path.
*   **Rust-native Development:** Built from the ground up in Rust to ensure memory safety and eliminate common kernel bugs.
*   **Interface specification:** The bootloader↔kernel contract — image format and load rules, entry ABI, `BootInfo`, handoff semantics and exit codes — is defined in [docs/architecture/kernel_interface.md](docs/architecture/kernel_interface.md).

### 🌐 Application Environment
*   **Web/Wasm Runtime:** The primary environment for user applications, providing a "software store" experience with near-native performance and total isolation.
*   **Minimal POSIX Layer:** A thin compatibility layer provided only for essential drivers and base system services.

## 🛠️ Getting Started

### Prerequisites

*   **Rust stable.** Verified with 1.98; nightly is *not* required.
*   Targets `x86_64-unknown-uefi` (the bootloader) and `x86_64-unknown-none` (the kernel).
*   **QEMU and OVMF** — only needed to run or test the image.

```bash
rustup target add x86_64-unknown-uefi x86_64-unknown-none

# Ubuntu / Debian
sudo apt install qemu-system-x86 ovmf
# Fedora / RHEL
sudo dnf install qemu-system-x86 edk2-ovmf
```

### Building

```bash
cargo build            # -> target/x86_64-unknown-uefi/debug/paranukos.efi
cargo build --release  # -> target/x86_64-unknown-uefi/release/paranukos.efi
```

The artifact is a **PE32+ EFI application** (`subsystem = 10`), which is what UEFI firmware can load. The target defaults to `x86_64-unknown-uefi` in `.cargo/config.toml`; `x86_64-unknown-none` produces an ELF image, which firmware cannot boot.

### Running in QEMU

```bash
cargo kernel   # build the kernel (bare-metal target) -> target/x86_64-unknown-none/debug/kernel
cargo run      # build the bootloader, place the kernel in the ESP, boot with OVMF
```

The runner builds a standards-compliant ESP (`/EFI/BOOT/BOOTX64.EFI`), places the kernel at the
agreed path (`\EFI\PARANUKO\KERNEL.ELF`) and boots it with OVMF. After the kernel prints its
`BootInfo` summary, QEMU either exits with a status code (when the `qemu-exit` feature is enabled)
or the CPU halts.

To leave the emulator, press **`Ctrl + A` and then `X`**.

If `Ctrl + A` does nothing at all, that QEMU's standard input is not your terminal — for example it
was started with the output redirected to a file, or launched as a background job. In that case run
one of these from another terminal, and open an issue if the runner did it to you:

```bash
pkill -f 'qemu-system-x86_64.*paranukos'   # or: kill $(pgrep -f qemu-system-x86_64)
```

`run-qemu.sh` runs QEMU in the **foreground** (`exec`), precisely so that the keyboard/mux input
stays connected to your terminal and so that killing the process cannot leave an orphan. Avoid
`Ctrl + C` by convention: use `Ctrl + A` `X`.

To leave the emulator, press **`Ctrl + A` and then `X`**. Do not use `Ctrl + C`: it leaves a zombie QEMU process holding the serial port.

### Kernel image

The kernel lives in `crates/kernel` (`x86_64-unknown-none`, `#![no_std]`, linked at `0x100000`). The
bootloader looks for it at `\EFI\PARANUKO\KERNEL.ELF` on the ESP, verifies that it is an
ELF64 / x86-64 / `ET_EXEC` image and loads it segment by segment at its link address.

At runtime the kernel installs its own IDT and then builds and installs its **own four-level
identity-mapped page tables** from the UEFI memory map in `BootInfo`: the first 2 MiB with 4 KiB
pages, everything above with 2 MiB blocks, and only addresses that overlap RAM. Non-RAM addresses
and page 0 are deliberately left not-present, so a stray or null access faults loudly (`#PF`)
instead of silently touching firmware memory or a device. The interface is specified in
[docs/architecture/memory_subsystem.md](docs/architecture/memory_subsystem.md).

### Tests

```bash
python3 tests/check_pe.py target/x86_64-unknown-uefi/debug/paranukos.efi  # static PE structure check
bash tests/smoke.sh                                                       # end-to-end QEMU smoke test
```

The smoke test asserts **exact QEMU exit codes** rather than "it printed something": 37 for a kernel
whose self-check passed, 35 for a loader image failure, 39 for a broken `BootInfo` contract, 43 for a
memory-initialisation failure, and 41 for a kernel fault. It also checks the serial log for evidence
(identity-mapping size, `BootInfo` still readable after the `CR3` switch, `#PF` naming vector 14 and
`cr2=0x0` for the deliberately unmapped null page).

### Known limitations

*   **Milestones 0–2a only.** The bootloader loads the kernel, calls `exit_boot_services` and jumps
    to the entry point; the kernel validates `BootInfo`, installs its own IDT and page tables, prints
    a summary on COM1 and exits. There is **no heap, no user mode, no IPC and no scheduler** yet —
    the ordering is listed in `docs/architecture/kernel_interface.md` §9, and M2b adds the physical
    frame allocator plus the kernel heap.
*   Only `ET_EXEC` kernel images are accepted; a PIE (`ET_DYN`) kernel would require relocation
    handling that is not implemented.
*   The kernel maps memory but does not use demand paging, W^X or per-process address spaces, and it
    deliberately does **not** map MMIO: it drives serial as port I/O and touches no MMIO device yet.

## 🗺️ Roadmap
- [ ] **Phase 1: Core Foundation** (IPC, Memory Management, Capability Model)
- [ ] **Phase 2: Hardware Abstraction** (ACPI Parsing, Block Device Drivers)
- [ ] **Phase 3: System Services** (Network Stack, CoW File System)
- [ ] **Phase 4: Application Runtime** (WASI Integration)

---
*ParanukOS - Stability through Isolation.*
