# ParanukOS

[English] | [中文](README_CN.md)

ParanukOS is a high-reliability, Rust-based microkernel operating system designed for modern hardware and secure application execution. By leveraging a microkernel architecture and a user-state focused service model, ParanukOS aims to build a robust platform that supports both low-level systems programming and high-level Web/Wasm applications.

## 🔗 Reference Project
This project is inspired by and references the architecture of [os.phil-opp.com](https://os.phil-opp.com/).

## 🌟 Vision & Philosophy
ParanukOS is built on the principle of **Strict Isolation**. We believe that system stability is achieved by minimizing the kernel's responsibilities and isolating every component—from drivers to file systems—into independent user-state services.

*   **Microkernel Core:** Only handles IPC, memory management, and basic scheduling.
*   **High Reliability:** A crash in a driver or file system service does not bring down the entire OS.
*   **Modern Hardware Focus:** Optimized specifically for the last 10 generations of hardware.
*   **Wasm-First Application Layer:** Provides a secure, sandboxed environment for users to run software via WebAssembly.

## 🚀 Key Features

### 🏗️ Microkernel Architecture
A pure Rust implementation of a microkernel that provides:
*   **Zero-copy IPC:** High-performance communication between system services.
*   **Capability-based Security:** Fine-grained permission management for all hardware resources.
*   **SMP Support:** Native multi-core scheduling and synchronization.

### 💾 File System Strategy
*   **Short-term:** A minimal, read-only Ext4/FAT32 compatibility layer for initial development and QEMU testing.
*   **Long-term:** A pure Rust, user-state **Copy-on-Write (CoW)** file system inspired by TFS, providing physical-level isolation of data writes.

### 🖥️ Hardware & Booting
*   **UEFI Bootloader (no GRUB):** A UEFI application built on [uefi-rs](https://github.com/rust-osdev/uefi-rs) 0.39 and compiled for `x86_64-unknown-uefi`. It locates the ESP it was started from, validates the kernel image as an ELF64/x86-64 executable and loads it into memory. GRUB is not used anywhere in the boot path.
*   **Rust-native Development:** Built from the ground up in Rust to ensure memory safety and eliminate common kernel bugs.

### 🌐 Application Environment
*   **Web/Wasm Runtime:** The primary environment for user applications, providing a "software store" experience with near-native performance and total isolation.
*   **Minimal POSIX Layer:** A thin compatibility layer provided only for essential drivers and base system services.

## 🛠️ Getting Started

### Prerequisites

*   **Rust stable.** Verified with 1.98; nightly is *not* required.
*   Targets `x86_64-unknown-uefi` (the bootloader) and `x86_64-unknown-none` (used by the smoke test to build a placeholder kernel image).
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
cargo run    # same as: ./run-qemu.sh target/x86_64-unknown-uefi/debug/paranukos.efi
```

The runner builds a standards-compliant ESP (`/EFI/BOOT/BOOTX64.EFI`) and boots it with OVMF.

To leave the emulator, press **`Ctrl + A` and then `X`**. Do not use `Ctrl + C`: it leaves a zombie QEMU process holding the serial port.

### Kernel image

The bootloader looks for the kernel at `\EFI\PARANUKO\KERNEL.ELF` on the ESP, verifies that it is an ELF64 / x86-64 / `ET_EXEC` image and copies it into freshly allocated pages. There is no real kernel in this repository yet — any valid ELF64 executable works for exercising the path (for example, building `tests/fixtures/dummy_kernel.rs`).

### Tests

```bash
python3 tests/check_pe.py target/x86_64-unknown-uefi/debug/paranukos.efi  # static PE structure check
bash tests/smoke.sh                                                       # end-to-end QEMU smoke test
```

The smoke test covers a positive case (a valid kernel image is present → boot and load must succeed) and a negative case (no kernel image → the loader must fail gracefully rather than hang, crash or report success).

### Known limitations

*   The bootloader does **not** call `exit_boot_services` or jump to the kernel entry point yet: after validating and loading the image it simply spins. Booting is therefore only asserted up to "image read, validated and loaded into memory".
*   Only `ET_EXEC` kernel images are accepted; a PIE (`ET_DYN`) kernel would require relocation handling that is not implemented.

## 🗺️ Roadmap
- [ ] **Phase 1: Core Foundation** (IPC, Memory Management, Capability Model)
- [ ] **Phase 2: Hardware Abstraction** (ACPI Parsing, Block Device Drivers)
- [ ] **Phase 3: System Services** (Network Stack, CoW File System)
- [ ] **Phase 4: Application Runtime** (WASI Integration)

---
*ParanukOS - Stability through Isolation.*
