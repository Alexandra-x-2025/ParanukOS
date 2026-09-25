# ParanukOS Vision

[English] | [中文](VISION_CN.md)

ParanukOS is a high-reliability, Rust-based microkernel operating system that aims to provide a secure and stable computing foundation for modern hardware. Through a microkernel architecture and a user-space service model, ParanukOS strives to build a solid platform that supports both low-level systems programming and high-level Web/Wasm applications.

## 🔗 Reference Project
This project is inspired by, and references the architecture of, [os.phil-opp.com](https://os.phil-opp.com/).

## 🌟 Core Philosophy
ParanukOS is built on the principle of **Strict Isolation**. We believe system stability comes from granting components the minimum possible authority:
*   **Microkernel architecture:** the kernel only handles the most fundamental tasks, such as IPC, memory management and basic scheduling.
*   **High reliability:** by moving all drivers and file systems into user mode, a crash in a single service cannot take the whole system down.
*   **Modern hardware focus (explicitly bounded):** x86-64 + UEFI + ACPI 6.x only — no BIOS/CSM, no 32-bit x86, no legacy device support. Stated as a technical constraint so it can actually decide trade-offs.
*   **Wasm-first application layer:** a secure sandbox that lets users run software through WebAssembly.

## 🚀 Key Features

### 🏗️ Microkernel Architecture
A microkernel implemented in pure Rust, providing:
*   **Zero-copy IPC:** a high-performance inter-process communication mechanism.
*   **Capability-based security:** fine-grained permission management for all hardware resources.
*   **SMP support:** native multi-core scheduling and synchronization.

### 💾 File System Roadmap
*   **Now:** FAT32 only. The ESP is already FAT32, so the bootloader (and an early kernel) can read the volume it was loaded from without writing a new file system driver.
*   **Later (requires its own interface document):** a pure-Rust, user-space **copy-on-write (CoW)** file system based on the TFS design, giving physical-level isolation of data writes.
*   **Explicitly not short-term: read-only Ext4.** Extents, the journal, indirect blocks and checksums make it a multi-month effort that buys almost nothing during bring-up, and no milestone in `docs/architecture/kernel_interface.md` depends on it.

### 🖥️ Hardware & Booting
*   **UEFI bootloader (no GRUB):** a UEFI application built on [uefi-rs](https://github.com/rust-osdev/uefi-rs) 0.39 and compiled for `x86_64-unknown-uefi`. It locates the ESP it was started from, validates the kernel image as an ELF64/x86-64 executable, loads it segment by segment at its link address, then calls `exit_boot_services` and passes control to the kernel entry point with a `BootInfo` structure. GRUB is not used anywhere in the boot path.
*   **Rust-native development:** built from the ground up in Rust, eliminating memory-safety vulnerabilities.
*   **Interface specification:** the bootloader↔kernel contract — image format and load rules, entry ABI, `BootInfo`, handoff semantics and exit codes — is defined in [docs/architecture/kernel_interface.md](docs/architecture/kernel_interface.md).

### 🌐 Application Environment
*   **Web/Wasm runtime:** the primary environment for user applications, offering a sandboxed "software store" experience with near-native performance.
*   **Minimal POSIX layer:** only the compatibility surface required by drivers and base system services.

## 🗺️ Roadmap
- [ ] **Phase 1: Core foundation** (IPC, memory management, capability model)
- [ ] **Phase 2: Hardware abstraction** (ACPI parsing, block device drivers)
- [ ] **Phase 3: System services** (network stack, CoW file system)
- [ ] **Phase 4: Application runtime** (WASI integration)

---
*ParanukOS - Stability through isolation.*
