# Contributing to ParanukOS

Welcome! We're thrilled that you want to help build **ParanukOS**, a high-reliability, Rust-based microkernel operating system. 

By contributing to this project, you are helping us build a foundation for secure, isolated, and stable computing. Please read this guide carefully before making your first contribution.

## 📜 Core Principles
Before writing code, please ensure you align with our core philosophy:
*   **Isolation First:** Every component (drivers, file systems, etc.) must be isolated in user-space.
*   **Microkernel Purity:** The kernel should only handle IPC, memory management, and scheduling.
*   **Rust Safety:** We use Rust to eliminate memory safety bugs. Adherence to `no_std` constraints is mandatory.

## 🛠️ Development Environment
To contribute effectively, you need the following tools:
*   **Rust Toolchain:** Rust Nightly (required for certain kernel features).
*   **QEMU:** For testing and debugging.
*   **Build Tools:** Ensure `cargo` and `make` (or your preferred build orchestrator) are installed.

### Setup
1. Clone the repository: `git clone <repo_url>`
2. Install dependencies as specified in the project's setup guide.
3. Run the environment check script to ensure your toolchain is ready.

## 💻 Coding Standards
*   **Language:** All core system code must be written in Rust.
*   **No Standard Library:** Kernel-space code must strictly follow `#![no_std]`.
*   **Memory Safety:** Avoid `unsafe` blocks unless absolutely necessary for hardware interaction. If used, provide a detailed comment explaining why it is safe.
*   **IPC Protocol:** All inter-service communication must use the defined IPC message formats and capability tokens.

## 🧪 Testing & Quality Assurance
We maintain high reliability by ensuring every change is verified:
1.  **Unit Tests:** Every new function or module should have corresponding unit tests.
2.  **Integration Tests:** Critical components (IPC, Memory Management) must be verified via QEMU integration tests.
3.  **Documentation:** Any new feature or significant architectural change must be documented in `docs/architecture/` and updated in the `README.md`.

## 🚀 Pull Request Process
1.  **Fork & Branch:** Create a feature branch from `main`.
2.  **Commit Messages:** Use clear, descriptive commit messages (e.g., `feat(ipc): add zero-copy message passing`).
3.  **Documentation:** Ensure all new APIs are documented in the project's documentation files.
4.  **Review:** Submit a PR. Be prepared for a thorough review focusing on:
    *   Memory safety and `unsafe` usage.
    *   Adherence to microkernel isolation principles.
    *   Performance impact of IPC calls.

## 🤝 Communication
We value clear communication. If you encounter blockers or have architectural questions, please discuss them in the project's designated communication channels before starting large-scale implementation.

---
*Thank you for helping us build a more stable and secure future with ParanukOS!*
