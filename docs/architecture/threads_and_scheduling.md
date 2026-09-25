# ParanukOS Threads and Scheduling — Milestone 3 Interface

[English] | [中文](threads_and_scheduling_CN.md)

> **Status: interface proposed, implementation not started.**
>
> Prerequisites: M0–M2 are implemented and verified ([kernel_interface.md](kernel_interface.md),
> [memory_subsystem.md](memory_subsystem.md)). This document defines what M3 delivers: **descriptor
> tables, interrupt infrastructure, locking, kernel threads and a preemptive round-robin scheduler
> skeleton** on a single core.

## 1. Scope

### 1.1 What this document defines
* The kernel's own GDT, TSS and IST stacks, and why M3 needs them (§3)
* Interrupt infrastructure: 8259 PIC remap, PIT, vector assignment, and the rules interrupt
  handlers must follow (§4)
* Locking: the interrupt-safe spinlock, what it protects, and the end of M2's "no locking" premise (§5)
* The thread model: what a thread is, its stack, its states, its lifecycle (§6)
* The context layout, the context-switch ABI, XMM handling, and how a new thread starts (§7)
* The scheduler: round-robin policy, ready queue, idle thread, reaping (§8)
* Kernel entry/exit protocol and the new exit code 45 (§9)
* The self-check that makes all of the above falsifiable (§10)
* Deliverables, the crate split and acceptance criteria (§11)

### 1.2 What this document does not define
User mode and syscalls (M4), per-thread address spaces, IPC and capabilities (M5), SMP, per-CPU data
and TLB shootdown, priority/deadline scheduling and load balancing, blocking primitives (`sleep`,
`join`, wait queues), stacks that grow on demand, guard pages, signals, and FPU/SIMD *lazy* switching.

### 1.3 Terminology

| Term | Meaning |
|---|---|
| thread | a schedulable execution context: an entry function, its own kernel stack, and saved register state |
| ready queue | the FIFO list of threads that may run; the running thread is not in it |
| idle thread | the thread that runs when the ready queue is empty; it `hlt`s |
| boot context | thread 0: whatever `kernel_main` itself runs on, using the stack the bootloader gave us |
| tick | one PIT interrupt, i.e. one scheduling opportunity |
| IST | Interrupt Stack Table: a stack in the TSS that the CPU switches to for a given vector |
| critical section | a region executed with interrupts disabled (`IF = 0`) |

## 2. Baseline and prerequisites

M3 consumes M2's `memory::MemoryLayout` (frames + heap) and M1's IDT/exception/logging
infrastructure. What it must not break:

* `BootInfo` and the entry ABI are unchanged; M3 adds no `BootInfo` field;
* exit codes 33/35/37/39/41/43 keep their meanings; 45 is new (§9.1);
* the bootloader is unchanged (the kernel image stays under `MAX_KERNEL_PAGES = 256`);
* the kernel still never touches MMIO (§4.1 explains how M3 avoids needing any).

> ⚠️ **M2 decision #20 ends here.** `memory_subsystem.md` §5.4 said "no locking; wrap it in a lock
> before enabling interrupts". M3 enables interrupts, so M3 is where that lock is added — for the
> frame allocator, the heap and the scheduler state (§5).

## 3. Descriptor tables: GDT, TSS, IST

M1 deliberately ran on the firmware's GDT and installed no IST ("needed before user mode", §9 of the
M1 decision record). M3 installs its own, because preemption turns the cost of a triple fault up: a
`#DF` raised while the kernel stack is unusable is exactly what IST exists to survive [decision #26].

* a static GDT with 8 entries: null, kernel code (64-bit, DPL 0), kernel data (DPL 0), a 16-byte TSS
  descriptor (two slots), and three spare slots reserved for M4's user code/data so that adding them
  later does not move the existing selectors;
* selectors stay the conventional `0x08` (code) and `0x10` (data), so reloading `CS`/`SS`/`DS`/`ES`
  after `lgdt` keeps the currently executing code valid;
* the TSS holds `rsp0` (used only if we ever take an interrupt from CPL 3 — M4) and two IST stacks:
  **IST1 for `#DF` (vector 8)** and **IST2 for NMI (vector 2)**. Each IST stack is one page from the
  frame allocator, with the same canary as a thread stack (§6.2);
* the IDT gates for `#DF` and NMI set the IST index; every other gate keeps IST 0 (the current stack).

The kernel asserts after `lgdt` that `sgdt` reports our table, and that `CS`/`SS` still carry the
expected selectors. A wrong GDT is an immediate triple fault, so this assertion is not optional.

`ltr` must load the TSS **before** any IST gate can be used, so the IDT is installed a second time
after the descriptor tables: the first install (M1, early) leaves every gate on the current stack, and
the second arms the IST indices for `#DF`/NMI [decision #37]. Until then a `#DF` would try to read a
TSS through an invalid task register and escalate instead of being diagnosed.

## 4. Interrupt infrastructure

### 4.1 8259 PIC + PIT, not the local APIC — deliberately
`memory_subsystem.md` §3.4 states that M2 maps **no MMIO** and that any later milestone touching the
local APIC must add explicit mappings first. The legacy 8259 PIC (`0x20`/`0x21`, `0xA0`/`0xA1`) and
the PIT (`0x40`–`0x43`) are driven purely by **port I/O**, so M3 gets a timer without touching the
paging design at all [decision #24]. The APIC is a separate, later change (it also requires
remapping MMIO and deciding cache attributes).

* remap the master PIC to vectors `0x20..0x27` and the slave to `0x28..0x2F` (ICW1–ICW4), because the
  firmware leaves them on `0x08..0x0F`, which collide with CPU exceptions;
* mask **every** IRQ except IRQ0 (the timer); the slave PIC is masked entirely;
* program PIT channel 0 in mode 3 (square wave) with divisor `1193182 / 100 = 11932`, giving
  `TIMER_HZ = 100` (the exact rate is 99.9984 Hz because the divisor is an integer; documented, not
  pretended otherwise);
* the kernel keeps a monotonic tick count in the scheduler state (§8.3).

### 4.2 Vectors

| Vector | Source | Handler |
|---|---|---|
| 0..31 | CPU exceptions (M1) | `exception_handler` → diagnostics → 41 |
| 2 (NMI) | NMI | on the IST2 stack; diagnostics → 41 |
| 8 (`#DF`) | double fault | on the IST1 stack; diagnostics → 41 (this is the whole point of IST) |
| 0x20 | IRQ0 / PIT | `timer_tick` → count, maybe reschedule (§7.3) → EOI |
| 0x21..0x2F | other IRQs | masked, never delivered; the gate points at a "spurious interrupt" handler that only prints and EOIs (defensive) |

### 4.3 Rules interrupt handlers must follow
1. **Never allocate.** The heap lock is safe to take from an ISR (interrupts are already disabled),
   but an allocation inside an ISR is a policy we do not want in M3; handlers must be allocation-free.
2. **Never yield, never block, never switch explicitly.** The only switch an ISR performs is the one
   the scheduler decides (§7.3).
3. **Always EOI** the PIC before returning (after the switch decision, before `iretq`) — otherwise
   IRQ0 stops arriving.
4. Interrupt gates (`IF = 0` on entry) are used throughout, so handlers never nest.

## 5. Locking

### 5.1 The lock
`crates/kernel/src/lock.rs` provides an **interrupt-safe spinlock**:

```rust
pub struct SpinLock<T> { locked: AtomicBool, value: UnsafeCell<T> }
impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self;
    pub fn lock(&self) -> SpinLockGuard<'_, T>;   // cli first, then spin on the flag
}

pub struct CriticalSection { /* saved RFLAGS.IF */ }
pub fn critical() -> CriticalSection;             // cli, remember whether IF was set

/// 当前线程持有的锁数量；yield/exit 用它拒绝"持锁切换"。
pub fn held_count() -> usize;
```

The order matters: **disable interrupts first, then acquire the flag.** The reverse order leaves a
window in which the interrupt handler could observe the lock as taken by a thread that has not
actually entered the critical section yet.

`SpinLockGuard` increments `held_count` on creation and decrements it on drop; dropping also releases
the flag and restores the saved `IF`.

### 5.2 What is protected, and by what
| State | Protection |
|---|---|
| scheduler state (thread table, ready queue, current thread, ticks) | `critical()` — on a single core, `IF = 0` **is** the scheduler's lock (§5.3), because the scheduler is also mutated from the timer ISR where a spinlock would deadlock |
| kernel heap (`#[global_allocator]`) | `SpinLock` around the free list |
| frame allocator | `SpinLock` around the bitmap |

### 5.3 Why the scheduler is protected by `IF = 0` and not by a lock
The timer ISR must be able to reschedule. If the scheduler were guarded by a spinlock, a thread
holding that lock would be preempted (interrupts are enabled while the heap lock is *not* held), the
ISR would try to take the same lock, and a single-core spinlock cannot make progress → deadlock.

So the rule is: **scheduler state is only ever touched inside a `critical()` section**, which means
interrupts are off, which means no timer can interleave. That makes the scheduler's critical sections
non-preemptible by construction.

Because the lock is not held across the switch, the "who unlocks after the switch" problem does not
arise. The invariant that makes this safe is:

> **Every context switch happens with `IF = 0`, and whichever context resumes is responsible for
> restoring `IF` (the thread's `CriticalSection` guard, or the ISR's `iretq`).**

A thread resumed inside `switch` therefore always continues from a point where interrupts were off,
and re-enables them exactly once, on the path that disabled them.

### 5.4 The one thing that would deadlock
Holding a heap/frame lock across a context switch: another thread would take the CPU and spin forever
on a lock that only the switched-out thread can release. M3 makes this a **panic (exit code 41)**
instead of a hang: `yield()` and `exit()` assert `held_count() == 0`. That check is itself part of the
self-check's evidence (§10).

## 6. Thread model

### 6.1 What a thread is
* an **entry function** `extern "C" fn() -> !`, or one that returns (in which case the trampoline
  calls `exit` for it);
* its **own kernel stack** from the frame allocator: `THREAD_STACK_PAGES = 4` (16 KiB), page-aligned;
* **saved register state** (§7);
* a **state**: `Unused`, `Ready`, `Running`, `Exited`;
* an id: a `usize` index into a fixed table of `MAX_THREADS = 16` entries.

All threads share **one address space** — the kernel's identity-mapped page tables from M2. There is
no per-thread `CR3` in M3 [decision #25]; that arrives with user mode (M4), where it is unavoidable.

### 6.2 Stacks, canaries and what is not defended
* the stack is `THREAD_STACK_PAGES` frames from the frame allocator, plus a **canary word** written
  at the lowest 8 bytes at creation, checked when the thread is reaped;
* there are **no guard pages** in M3: the identity map uses 2 MiB blocks above 2 MiB, so unmapping a
  single 4 KiB page would mean splitting a block and allocating a page table — a change to the paging
  interface that this milestone does not need [decision #32]. An overflow that skips the canary (a
  large `memcpy`, a deep recursion) is therefore still possible; the canary catches the common case;
* the idle thread's stack and the two IST stacks are allocated the same way; the **boot context
  (thread 0) uses the bootloader's stack** and its frames are never freed (§6.4).

### 6.3 Lifecycle

```
                create()                schedule()                exit()
   Unused ──────────────▶ Ready ──────────────────────▶ Running ──────────▶ Exited
                            ▲                              │                  │
                            └──────── preempt / yield ─────┘                  │
                                                                              ▼
                                                                    reaped by the scheduler
                                                                    (state → Unused, stack freed)
```

* `create(entry)` allocates a stack, writes the canary, builds the initial context, and appends the
  thread to the ready queue. Failure (no free slot, no frames) is reported to the caller as `Err`;
* `exit()` marks the running thread `Exited`, then switches away **for good**; the scheduler reaps it
  later (§8.3) — a thread can never free the stack it is standing on;
* a thread that returns from its entry function is treated as exiting (the trampoline calls `exit`);
* `Exited` threads are not scheduled again; their `ThreadId` becomes reusable only after reaping.

### 6.4 Thread 0: the boot context
`kernel_main` runs on the bootloader-provided stack with interrupts disabled, so it is *already* a
context that can be switched away from and back to. M3 registers it as **thread 0 ("boot")**:

* its entry is the remainder of `kernel_main` — it is never "started" through the trampoline;
* its stack comes from `BootInfo` and is never freed;
* it participates in scheduling like any other thread (it can be preempted while spinning, §10.4);
* when it finishes, it disables interrupts and exits the machine (§9).

## 7. Context and context switching

### 7.1 `Context`
```rust
#[repr(C)]
pub struct Context {
    pub rsp: u64,       // 保存的栈指针（指向保存的 rbx/rbp/r12-r15 与返回地址）
    pub thread: u32,    // 自检与诊断用
    pub _pad: u32,
}
```
The rest of a thread's state lives **on its own stack**, which is what makes the switch small: the
callee-saved registers and the resume address are pushed there before `rsp` is swapped.

### 7.2 The switch ABI
```rust
/// # Safety
/// `from`/`to` 必须指向各自线程的 `Context`，且必须在 `IF = 0` 下调用。
pub unsafe fn switch(from: *mut Context, to: *const Context);
```
`switch` is ~15 instructions: push `rbx`, `rbp`, `r12`–`r15`; store `rsp` into `*from`; load `rsp`
from `*to`; pop `r15`–`rbx`; `ret`. The `ret` returns into whoever last called `switch` on that
stack [decision #28].

Consequences that matter:

* a thread that was switched out is resumed *inside its own call to `switch`*, so from Rust's point of
  view `switch` simply "took a while";
* **the switch never happens inside a lock** (§5.4) and always with `IF = 0`;
* `switch` must be called on the *current* thread's stack; it is not reentrant and not callable from
  two places at once (single core: guaranteed by `IF = 0`).

### 7.3 Who calls `switch`
| Path | Initiator | Notes |
|---|---|---|
| cooperative | `yield()` from a thread | `critical()` + scheduler decision + `switch`; the guard restores `IF` after the thread is resumed |
| preemptive | the timer ISR | `critical` is implicit (the gate cleared `IF`); the handler counts the tick, asks the scheduler, and switches; after the switch returns, it EOIs and the stub `iretq`s, which restores `IF` |
| exit | `exit()` from a thread | marks `Exited`, then switches away; never returns |
| start | the scheduler | switching to a brand-new thread "returns" into the trampoline instead of a previous `switch` call |

### 7.4 Starting a new thread
The initial stack is built so that `switch`'s `pop`/`ret` sequence lands in
`thread_trampoline`, with the thread's entry point and argument placed where the trampoline looks for
them:

* stack contents, from the top down: an **FXSAVE area** (§7.5), the trampoline return address, six
  dummy callee-saved slots, then the entry function pointer and a slot the trampoline reads;
* `rsp` at trampoline entry satisfies `rsp % 16 == 8`, exactly as if a `call` had been made, so the
  trampoline can call Rust code without re-aligning;
* the trampoline reads the entry function, calls it, and calls `exit()` if it ever returns.

### 7.5 XMM state: saved on the thread's stack, not in a per-thread area
The kernel is not float-free: the compiler may use XMM registers for large struct moves and for
`memcpy`-ish loops, and nothing in the source says so. A preempted thread whose XMM registers were
clobbered by the incoming thread corrupts data **non-deterministically**, which is the worst class of
bug. M3 therefore handles XMM explicitly [decision #27]:

* the **timer ISR stub** executes `fxsave` into the current thread's stack (16-byte aligned, below the
  saved GPR frame) *before* calling any Rust code;
* the **ISR epilogue** executes `fxrstor` from the same place before restoring the GPRs, so whichever
  thread the switch resumed gets its own XMM state back;
* **no per-thread FXSAVE area is needed**: a thread's XMM state lives on its own stack, exactly like
  its GPRs. A new thread's crafted stack contains a zeroed area with `MXCSR = 0x1F80` (the standard
  default);
* the cooperative `yield()` path needs no XMM handling at all: at a function call boundary, XMM
  registers are caller-saved (dead), so there is nothing to preserve.

The alternative — "kernel threads must not use SSE" — was rejected because the compiler, not the
programmer, decides when XMM is used.

## 8. Scheduler

### 8.1 Policy
**Strict round-robin** over a FIFO ready queue [decision #31]:

* the ready queue is a ring buffer of `ThreadId`s with capacity `MAX_THREADS`;
* when the running thread is preempted or calls `yield`, it is appended to the **tail** and the head
  is popped as the new running thread;
* if the queue is empty and the current thread is not idle, switch to the **idle thread**; if the
  idle thread is already running, stay on it (it `hlt`s until the next tick, so an idle machine burns
  no CPU);
* the idle thread is **not** in the ready queue and is never reaped;
* priorities, deadlines, fairness accounting and load balancing are explicitly out of scope; they are
  a later milestone with its own document.

### 8.2 Where the logic lives
`crates/kernel-sched` (new, zero dependencies, `no_std`, `#![forbid(unsafe_code)]`) owns the pure
state machine: the thread table, the ready queue, the tick counter, `create`/`exit`/`yield`/
`pick_next` decisions and the reaping candidate list. It operates on storage the kernel hands it
(exactly as `kernel-memory` does with the bitmap), so it is covered by host unit tests. Everything
that needs a CPU — `switch`, port I/O, `fxsave`, the IDT — stays in `crates/kernel`.

### 8.3 Tick accounting and reaping
* every timer interrupt increments the tick counter (inside the critical section);
* the scheduler returns a decision: `Keep` (no switch) or `Switch(ThreadId)`;
* after a switch, the scheduler scans for `Exited` threads and reports their stacks as reapable; the
  kernel frees those frames and resets the slot to `Unused`. A thread's own stack is never freed while
  it runs, because `exit` switches away first (§6.3).

## 9. Kernel entry, exit protocol and exit codes

### 9.1 New exit code

| Code | Meaning | Reported by | Status |
|---|---|---|---|
| **45** | **Kernel scheduler self-check failed** — the descriptor tables or interrupt infrastructure could not be initialised, the timer never ticked, a thread could not be created, the round-robin order was wrong, a preempted thread made no progress, the lock did not provide mutual exclusion, or a stack canary was destroyed | kernel | new in M3 |

41 stays "the kernel crashed" (CPU exception or panic); 43 stays "memory initialisation failed". The
three have different owners, which is the whole point of keeping them apart.

### 9.2 Interrupt enable/disable timeline
1. everything from M0 to M2 runs with `IF = 0`, as before;
2. GDT/TSS/IST, PIC, PIT and the scheduler are initialised with `IF = 0`;
3. `sti` happens **once**, immediately before the first non-boot thread is created — i.e. after every
   subsystem that an interrupt handler might touch is ready;
4. the self-check runs with interrupts enabled (it has to: it tests preemption);
5. the exit protocol disables interrupts again before reporting.

### 9.3 Exit protocol
After the self-check passes: `cli` → mask IRQ0 (PIC) → assert the ready queue is empty and no thread
is `Running` except the boot context → print the summary → write the exit port (37). A machine that
reaches `finish()` with threads still runnable is a kernel bug, so the assertion is part of the
protocol rather than a comment. In interactive mode (`exit_port == 0`) the kernel instead `hlt`s with
interrupts disabled, so the emulator stays usable.

## 10. Self-check (the evidence made machine-checkable)

Runs in `kernel_main` after the M2 memory self-check, with interrupts enabled. Every step prints its
outcome and a failure exits **45** with the step number, the invariant and the address/data involved.

1. **Mutual exclusion** — 4 threads, each performing 2000 increments of a shared counter *under the
   heap lock's sibling* (the same `SpinLock` implementation, a dedicated lock for the test). The final
   value must be exactly 8000; a lost update from a preemption inside the critical section shows up
   immediately.
2. **Round-robin order (cooperative)** — 3 threads, each appending its id to a shared byte array and
   then calling `yield()`, for 10 rounds. The observed sequence must be exactly `012012…`; any
   deviation (a thread running twice in a row, a missed wakeup) is a failure.
3. **Preemption** — a thread **A** busy-waits (never yielding) for a flag that only thread **B** can
   set, with a bounded budget of ticks. A second thread that only becomes runnable if the timer
   preempts A is the whole point: if preemption is broken, A never sees the flag, its budget expires
   and the check fails cleanly (45) instead of hanging.
4. **Ticks moved** — during step 3 the tick counter must have advanced by at least the number of
   switches A observed; a stuck PIT would leave the counter at 0 and fail here rather than time out.
5. **Exit, reaping and accounting** — every test thread exits; afterwards the scheduler must hold no
   `Exited` thread, every stack canary must still be intact, the frame allocator's free count must be
   back to its pre-thread value, and the only `Running` thread must be the boot context.
6. **Lock discipline** — `held_count()` must be 0 at every scheduling point, and the self-check
   verifies the guard's accounting directly: taking the test lock must report 1 (so a `yield` at that
   moment *would* be rejected), and dropping it must report 0 again. The panic path itself
   (`yield` with a lock held → 41) is deliberately **not** triggered here, because a self-check
   failure must be reported as 45, not as a crash.

> Note on `int $8`: it was the first attempt at this injection and it does switch to IST1, but a
> **software** `int n` never pushes an error code, so the frame is misparsed and every field in the
> diagnostic is garbage. The injection was therefore changed to a real double fault (a fault while
> delivering a fault) so that the evidence — not just the exit code — is trustworthy [decision #36].

Two test-only features make the failures reachable rather than theoretical [decision #35]:

| Feature | Effect | Expected |
|---|---|---|
| `inject-no-preempt` | the timer keeps counting but the scheduler never switches to another thread | self-check step 3 fails → **45** |
| `inject-double-fault` | corrupt the `#PF` and `#GP` gate selectors, then read an unmapped address: delivering `#PF` fails, delivering `#GP` fails, and the CPU raises a **genuine** `#DF` | the `#DF` gate switches to IST1, the handler prints a complete frame (`error_code=0x0`, the faulting `rip`, the pre-fault `rsp`) plus `实际运行栈：IST1 栈`, then exits **41**; a triple fault instead would be a timeout (124), i.e. a visible failure |

## 11. Delivery plan, deliverables and acceptance criteria

### 11.1 Split

| Part | Content | Why |
|---|---|---|
| **M3a** ✅ (PR #19) | `crates/kernel-sched` (host-tested, pure); GDT/TSS/IST; PIC + PIT; locks; timer ISR wired to the tick counter (no switching yet) | the descriptor tables and the interrupt plumbing are where a mistake is a triple fault or a silent storm; proving "the timer ticks, the IDT is ours, `#DF` lands on IST" is a milestone of its own |
| **M3b** | `Context` + `switch` + trampoline; thread create/exit/yield; the round-robin scheduler in the timer path; reaping; the §10 self-check; the two injection features; smoke cases | builds on a proven interrupt path, and the switching itself is small enough to be reviewed as a unit |

### 11.2 Deliverables

| # | Deliverable | Notes |
|---|---|---|
| 1 | `crates/kernel-sched` | zero dependencies, `no_std`, `forbid(unsafe_code)`, host unit tests for create/exit/yield/round-robin/reap/tick |
| 2 | `crates/kernel/src/gdt.rs` | GDT + TSS + 2 IST stacks, `lgdt`, selector reload, `sgdt` self-assertion |
| 3 | `crates/kernel/src/pic.rs` | 8259 remap/mask (+EOI) and PIT programming, port I/O only |
| 4 | `crates/kernel/src/lock.rs` | `SpinLock`, `CriticalSection`, `held_count` |
| 5 | `crates/kernel/src/thread.rs` | `Context`, `switch`, trampoline, stack allocation, canary |
| 6 | `crates/kernel/src/sched.rs` | the machine side: statics, ISR glue, yield/exit/create, reaping, self-check |
| 7 | `crates/kernel/src/idt.rs` | IRQ vector 0x20, spurious IRQ gates, IST gates for `#DF`/NMI |
| 8 | Exit code 45 + 2 injection features | §9.1, §10 |
| 9 | `tests/smoke.sh` | new cases (§11.3) |
| 10 | Docs | this document, `kernel_interface.md` §7.2/§9, `memory_subsystem.md` §5.4 (decision #20 is superseded), README — all bilingual |

### 11.3 Acceptance criteria — machine-checkable
The status marker says which part delivers each item.

- [x] (M3a) the kernel logs `sched: N 个线程就绪，PIT 100 Hz，GDT/TSS 已装载`;
- [x] (M3a) `gdt: GDT/TSS 已装载（CS=0x8 SS=0x10，TSS=…，IST1=…，IST2=…）` — selectors unchanged,
      both IST stacks non-zero;
- [x] (M3a) `pic: 8259 已重映射到 0x20..0x2F，PIT 分频 11932（100 Hz），只放行 IRQ0`;
- [x] (M3a) with interrupts enabled the tick counter reaches ≥ 3 inside a bounded spin budget, i.e.
      the PIT and IRQ0 really work;
- [x] (M3a) a genuine `#DF` lands on IST1 and prints a complete frame — `inject-double-fault` → **41**;
- [x] (M3a) the frame-allocator allocate/free round trip returns to the exact same free count while
      the new lock is in use;
- [x] (M3a) no regression in the existing assertions (37/35/35/39/41/41/43);
- [x] (M3a) `cargo test -p kernel-sched …` passes; clippy/fmt clean in every configuration, including
      `inject-double-fault`;
- [x] (M3a) `check_kernel_elf.py` passes (the image grew to 109 pages, budget 256);
- [ ] (M3b) self-check steps 1–6 pass on real QEMU + OVMF (all six are assertions, not prints);
- [ ] (M3b) the boot context observed at least one preemption during step 3;
- [ ] (M3b) after step 5 the frame allocator's free count is exactly its pre-thread value (no leaks);
- [ ] (M3b) `inject-no-preempt` exits **45**; the smoke test grows to ≥ 42 assertions.

Measured on QEMU 8.2.2 + OVMF (M3a, 43/43 smoke assertions green):

```
gdt: GDT/TSS 已装载（CS=0x8 SS=0x10，TSS=0x123110，IST1=0x26F000，IST2=0x270000）
pic: 8259 已重映射到 0x20..0x2F，PIT 分频 11932（100 Hz），只放行 IRQ0
sched: 1 个线程就绪，PIT 100 Hz，GDT/TSS 已装载
timer: 观察到 3 次 tick（0 次调度决策），中断已按退出协议关闭
```

and for the injected double fault:

```
未处理的 CPU 异常: #DF 双重故障 (vector 8)
  error_code=0x0 rip=0x1015FA cs=0x8 rflags=0x6
  rsp=0x6005420 ss=0x10
  实际运行栈：IST1 栈（#DF）
```

### 11.4 Known pitfalls, most likely first

| # | Pitfall | Mitigation |
|---|---|---|
| 1 | A wrong GDT triple-faults instantly | `lgdt` then `sgdt` self-assertion and selector checks before anything else (§3) |
| 2 | PIC not remapped → the timer collides with exception vectors | remap first, mask everything but IRQ0, and assert that an unmasked IRQ never arrives |
| 3 | Forgetting the EOI → exactly one tick | EOI in the handler, counted by the self-check (ticks must keep growing) |
| 4 | Switching while a lock is held → single-core deadlock | `held_count` panic + the "IF = 0 is the scheduler lock" rule (§5.4) |
| 5 | XMM clobbered across a preemption | `fxsave`/`fxrstor` in the timer stub on the thread's own stack (§7.5) |
| 6 | Stack alignment wrong at thread start → `#GP` on the first `movaps` | the crafted stack makes the trampoline see `rsp % 16 == 8`, and the trampoline re-checks it |
| 7 | Freeing a thread's stack while it still runs on it | `exit` switches away first; only the scheduler reaps (§6.3) |
| 8 | A thread that never yields hangs the machine | the preemption check has a bounded tick budget and reports 45 (§10.3) |
| 9 | Re-entering `switch` from an ISR while a thread is mid-`switch` | `IF = 0` for the whole switch, and the timer gate does not nest (§4.3) |
| 10 | Treating the PIT divisor as exact | documented 99.9984 Hz; the self-check never assumes an exact rate |

## 12. Decision record

| # | Decision | Outcome | Rationale | Reversibility |
|---|---|---|---|---|
| 24 | Preemptive scheduling from the start of M3, driven by the 8259 PIC + PIT | port I/O only, 100 Hz, plus a cooperative `yield` | a scheduler without preemption is not one, and M4 needs timer preemption anyway; PIC/PIT avoids reviving the "no MMIO" question that the local APIC would force | medium (the APIC replaces it later) |
| 25 | Kernel threads share one address space | thread = entry + stack + saved registers; no per-thread `CR3` | "single-core kernel thread skeleton"; per-thread address spaces belong with user mode, where the hardware requires them | high |
| 26 | Own GDT + TSS + IST for `#DF`/NMI in M3 | 6-entry GDT, two IST stacks | preemption makes an unusable-stack double fault much more likely, and an IST turns a silent triple-fault reboot into a diagnosable crash | high |
| 27 | Save/restore XMM on the thread's own stack, in the timer ISR path | `fxsave`/`fxrstor` in the stub; nothing in the `yield` path | the compiler, not the programmer, decides when XMM is used; a per-thread FXSAVE area is unnecessary because the stack already holds the thread's state | high |
| 28 | `switch(from, to)` push/pop design, new threads bootstrapped with a crafted stack | ~15 instructions of asm; resume happens inside the previous `switch` call | smallest correct mechanism; unifies yield, preemption and thread start | medium (an ABI-like detail) |
| 29 | `IF = 0` (a `CriticalSection`) is the scheduler's lock; heap and frame allocator get an irq-safe `SpinLock` | no lock is ever held across a switch | a single-core spinlock taken by the scheduler would deadlock when the timer preempts a holder; `IF = 0` cannot | medium |
| 30 | Interrupt handlers never allocate, never block; `yield`/`exit` with a lock held panics | `held_count`, checked at every scheduling point | converts the one deadlock M3 could produce into a loud, testable failure | high |
| 31 | Strict round-robin over a FIFO ready queue | preempted/yielding threads go to the tail | a skeleton should be the simplest thing that is still fair; priorities are a later milestone with their own interface | high |
| 32 | No guard pages; canary words instead | 4-page stacks with a canary checked at reap | guard pages need single-page unmapping, which needs block splitting in the paging interface — out of scope here | high |
| 33 | Thread 0 is the boot context on the bootloader's stack | never freed, never started through the trampoline | `kernel_main` is already a context; wrapping it costs nothing and keeps the switch path uniform | high |
| 34 | New exit code 45 for scheduler self-check failure | distinct from 39/41/43 | "the scheduler misbehaved" is neither a contract break, a crash, nor a memory failure | medium (released meanings are frozen) |
| 35 | Injections: `inject-no-preempt` → 45 and `inject-double-fault` → 41 | both reachable from `tests/smoke.sh` | the two claims most worth falsifying are "the timer really preempts" and "a `#DF` lands on IST instead of triple-faulting" | high |
| 36 | The `#DF` injection is a **genuine** double fault (corrupt the `#PF`/`#GP` gate selectors, then touch an unmapped address), not `int $8` | complete, trustworthy frame in the diagnostic | a software `int n` pushes no error code, so `int $8` misparses the frame and prints garbage — the claim would then be "proven" by an exit code alone | high |
| 37 | The IDT is installed twice: once early (M1, every gate on the current stack) and again after the TSS is loaded, when the `#DF`/NMI gates are armed with their IST indices | IST gates only exist once a valid task register does | an IST gate without a loaded TSS escalates to a triple fault, which is exactly what IST is supposed to prevent | high |

## 13. Interface change process
1. `crates/kernel-sched`'s public items are an interface between the kernel and its own logic; changes
   must come with host tests.
2. Anything that changes the **bootloader↔kernel** contract (`BootInfo`, entry ABI, exit codes)
   follows `kernel_interface.md` §11.
3. The context-switch ABI (§7) is internal but ABI-like: changing it means changing `thread.rs`,
   the trampoline and the ISR stub together.
4. Bilingual docs: every change must update both this file and
   [threads_and_scheduling_CN.md](threads_and_scheduling_CN.md) (AGENTS.md rule 4).
5. Implementation status: after each part, update §11 and the M3 row of `kernel_interface.md` §9.
