# ParanukOS Contributing Guide

[English] | [中文](CONTRIBUTING_CN.md)

Welcome to **ParanukOS**! We are delighted that you want to help build this high-reliability, Rust-based microkernel operating system.

By contributing you are helping us establish a secure, stable and extensible computing foundation. Please read this guide carefully before submitting any code.

## 📜 Core Principles
Before writing code, make sure your work matches our core philosophy:
*   **Isolation First:** every component (drivers, file systems, …) must run in user mode wherever possible, isolated from the others.
*   **Microkernel Purity:** the kernel should only handle the most fundamental tasks, such as IPC, memory management and basic scheduling.
*   **Rust Safety:** we use Rust to eliminate memory-safety bugs. Strictly honouring the `#![no_std]` constraint is mandatory.

## 🛠️ Development Environment
You need the following to contribute efficiently:
*   **Rust toolchain:** Rust **stable** — verified with 1.98. Nightly is *not* currently required.
*   **Targets:** `x86_64-unknown-uefi` (the bootloader itself) and `x86_64-unknown-none` (used by the smoke test to build a placeholder kernel image).
*   **QEMU and OVMF:** needed to run and test the image.
*   **Build tools:** `cargo` plus the usual compilation dependencies.

### Setup
1.  Clone the repository: `git clone <repo_url>`
2.  Add the required targets: `rustup target add x86_64-unknown-uefi x86_64-unknown-none`
3.  Install QEMU and OVMF: `sudo apt install qemu-system-x86 ovmf` (Debian/Ubuntu) or `sudo dnf install qemu-system-x86 edk2-ovmf` (Fedora/RHEL).

Build, run and test commands are documented in [README.md](README.md).

## 💻 Code Style
*   **Language:** all core system code must be written in Rust.
*   **No standard library:** kernel-space code must strictly follow `#![no_std]`.
*   **Memory safety:** avoid `unsafe` blocks unless they are genuinely required to touch hardware. If you must use one, document why it is sound.
*   **IPC protocol:** all inter-process communication must follow the project's IPC message format and capability tokens.

## 🧪 Tests and Quality Assurance
We maintain reliability through strict verification. Before opening a pull request, make sure all of the following pass — they are exactly what CI runs:

1.  **Static checks:** `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings`.
2.  **Image check:** `python3 tests/check_pe.py target/x86_64-unknown-uefi/debug/paranukos.efi` verifies that the artifact is a loadable PE32+ EFI application (structure-level, not a `file(1)` string match).
3.  **Boot tests:** `bash tests/smoke.sh` boots the image under QEMU/OVMF and asserts both the success path (a valid kernel image is found, validated and loaded) and the failure path (no kernel image → graceful error).
4.  **Unit tests:** `[[bin]] test = false` is required for this UEFI binary — a `std`-based test harness conflicts with the UEFI panic handler and fails to compile with a duplicate `panic_impl` lang item. Logic worth unit-testing should therefore live in dependency-free modules or crates that can be built and tested for the host target.
5.  **Documentation:** every new feature or significant architectural change must update the docs under `docs/architecture/` — and, per `AGENTS.md`, keep the Chinese (`_CN`) counterpart in sync.

## 🚀 Pull Request Process
1.  **Fork and branch:** create a topic branch from `main`.
2.  **Commit messages:** use clear, descriptive messages (for example `feat(ipc): add zero-copy message passing`).
3.  **One concern per pull request:** keep changes focused so they can be reviewed and reverted independently.
4.  **Documentation sync:** make sure any new API is documented.
5.  **Code review:** once the PR is open we focus on:
    *   memory safety and the justification for any `unsafe` usage;
    *   conformance with the microkernel isolation principles;
    *   the performance impact of IPC calls.

## 🤝 Communication
We value clear communication. If you hit a technical obstacle or have an architectural question, please discuss it through the project's channels before starting large-scale work.

---
*Thank you for helping us build a more stable and more secure ParanukOS!*
