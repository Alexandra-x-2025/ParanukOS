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
*   **Refactored GRUB:** A custom Rust-based bootloader/refactor that supports only the last 10 generations of hardware, reducing complexity and increasing security.
*   **Rust-native Development:** Built from the ground up in Rust to ensure memory safety and eliminate common kernel bugs.

### 🌐 Application Environment
*   **Web/Wasm Runtime:** The primary environment for user applications, providing a "software store" experience with near-native performance and total isolation.
*   **Minimal POSIX Layer:** A thin compatibility layer provided only for essential drivers and base system services.

## 🛠️ Getting Started

*(Development in progress - Build instructions will be updated as the toolchain matures)*

### Prerequisites
*   Rust (Nightly)
*   QEMU
*   `run-qemu.sh` script

### Building the Kernel
```bash
# Example command (to be finalized)
cargo build --release
```

## 🗺️ Roadmap
- [ ] **Phase 1: Core Foundation** (IPC, Memory Management, Capability Model)
- [ ] **Phase 2: Hardware Abstraction** (ACPI Parsing, Block Device Drivers)
- [ ] **Phase 3: System Services** (Network Stack, CoW File System)
- [ ] **Phase 4: Application Runtime** (WASI Integration)

---
*ParanukOS - Stability through Isolation.*
