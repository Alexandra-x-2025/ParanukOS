# ParanukOS Microkernel Core Architecture

[English] | [中文](core_kernel_CN.md)

## 1. Design Goals
*   **Minimal kernel mode:** the kernel only handles the most fundamental hardware abstraction, task scheduling and inter-process communication (IPC).
*   **High reliability:** by moving every driver and system service into user mode, a crash in a single service cannot take the whole system down.
*   **Rust safety:** leverage Rust's ownership model to eliminate memory-safety bugs, and build a deliberately small kernel in a `no_std` environment.

## 2. Core Components

### A. IPC Mechanism
IPC is the soul of a microkernel. ParanukOS will use **capability-based asynchronous message passing**:
*   **Zero-copy:** transfer large payloads between user-space services through shared memory pages.
*   **Hybrid sync/async:** provide a simple synchronous call interface (for driver interaction) and high-performance asynchronous message queues (for the network stack and file systems).

### B. Memory Management
*   **Page table management:** the kernel maintains the physical-to-virtual memory mappings.
*   **Capability model:** processes never own hardware resources directly; they hold capability tokens instead. A driver, for example, must hold a specific "I/O port access" capability before it can touch hardware.

### C. Scheduling
*   **SMP support:** symmetric multiprocessing, so user-space services can run in parallel on different cores.
*   **Priority scheduling:** system-critical services (such as the block device driver) get high-priority guarantees.

## 3. User-space Services
Every non-core function runs as an independent process:
*   **Block Device Service:** physical disk I/O and low-level sector management.
*   **CoW File System Service:** logical volumes and copy-on-write handling in user space, based on the TFS design.
*   **Network Stack Service:** an independently running TCP/IP stack.
*   **Driver Services:** hardware drivers including graphics and network adapters.

## 4. Isolation Model
*   **System space vs. application space:** kernel mode and user mode are strictly separated by page tables.
*   **Service-to-service isolation:** each user-space service runs in its own address space and can only communicate through controlled IPC channels.
*   **Wasm sandbox:** user applications run entirely inside a Wasm environment and request system services through WASI.

## 5. Next Steps
1.  [ ] Define the IPC message format and protocol specification.
2.  [ ] Design the capability-based permission model.
3.  [ ] Implement the basic memory page management logic.
