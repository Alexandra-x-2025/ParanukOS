# ParanukOS Memory Subsystem — Milestone 2 Interface

[English] | [中文](memory_subsystem_CN.md)

> **Status: interface proposed, implementation not started.**
>
> Prerequisite: M0 (handoff + self-check) and M1 (IDT / panics / logging) are complete — see
> [kernel_interface.md](kernel_interface.md). This document defines what M2 delivers: the
> **kernel's own page tables, a physical frame allocator and a kernel heap**. It is the interface
> that [kernel_interface.md](kernel_interface.md) §9 pointed at as "each milestone needs its own
> interface document".

## 1. Scope

### 1.1 What this document defines
* The kernel's own page tables: identity-map policy, granularity, attributes, storage, installation (§3)
* Kernel-side parsing of the UEFI memory map (§4)
* The physical frame allocator: free/in-use policy, granule, API contract, limits (§5)
* The kernel heap: placement, algorithm, `#[global_allocator]` wiring, failure semantics (§6)
* Exit code 43 and the fault-injection features that make it testable (§7)
* The M2a/M2b delivery split, deliverables and falsifiable acceptance criteria (§8)

### 1.2 What this document does not define
Virtual memory beyond a flat identity map (no higher-half layout, no per-process address spaces,
no demand paging, no copy-on-write), MMIO mappings (no APIC / HPET / framebuffer), user-mode page
permissions and W^X (M4), SMP and TLB shootdown, memory hotplug, NUMA, swapping, ACPI table
parsing, heap growth, and kernel stack guard pages.

### 1.3 Terminology

| Term | Meaning |
|---|---|
| identity map | virtual address == physical address |
| PML4 / PDPT / PD / PT | the four levels of x86-64 4-level paging |
| 2 MiB block | a PD entry with `PS=1`, mapping 2 MiB with a single entry |
| frame | a 4 KiB physical page — the allocator's granule |
| RAM region | a memory-map region the kernel is willing to map as normal memory (§3.4) |
| `phys_limit` | the highest physical address the kernel maps, clamped by `MAX_IDENTITY_BYTES` |

## 2. Baseline and prerequisites

M0 and M1 are implemented and merged. M2 consumes these `BootInfo` v0 fields:
`mmap_ptr` / `mmap_len` / `mmap_desc_size` / `mmap_desc_ver`, `kernel_base` / `kernel_size`,
`stack_top` / `stack_size`, `rsdp`, `exit_port`.

What M2 inherits and must not break:

* the entry ABI (`kernel_interface.md` §4) is unchanged and **`BootInfo` is not redefined** — M2 adds no field;
* exit codes 33 / 35 / 37 / 39 / 41 keep their meanings; 43 is new (§7.1);
* the bootloader's load rules are unchanged except the page budget `MAX_KERNEL_PAGES` (§8.1);
* serial logging (`kinfo!` / `kwarn!` / `kerror!`) stays the only output channel.

## 3. The kernel's own page tables

### 3.1 Why M2 builds page tables at all
`kernel_interface.md` §4 already warns that the kernel "must not rely on" the firmware's identity
mapping, and §6.2 repeats that it is only *usually* there. M2 is the first milestone that **uses**
physical memory it did not receive from the bootloader (frames backing the heap), so from M2 on the
mapping is load-bearing. Building our own tables removes that dependency [decision #11] and is a
prerequisite for user mode (M4).

### 3.2 Paging mode
4-level paging — exactly the mode OVMF already enabled. Before building anything the kernel asserts
`CR4.PAE = 1`, `CR4.LA57 = 0` (5-level not active) and `EFER.LMA = 1`; a mismatch is a memory-init
failure (43). The kernel never disables paging, never writes `CR0`/`CR4` and never enables
`CR4.PGE`.

### 3.3 Granularity
* the first 2 MiB (`0x0..0x20_0000`): **4 KiB pages**, through one page table;
* everything above: **2 MiB blocks**.

Rationale: 2 MiB blocks keep table storage small (4 KiB per GiB of address space) and, above all,
**static** — no allocator is needed to bootstrap the mapping. The first 2 MiB is the exception
because it contains the low-memory holes (VGA/BIOS) and because we want the null page *deliberately*
unmapped (§3.4).

### 3.4 Which addresses are mapped

Definitions:

* `is_ram(type)` is true for `EfiLoaderCode(1)`, `EfiLoaderData(2)`, `EfiBootServicesCode(3)`,
  `EfiBootServicesData(4)`, `EfiRuntimeServicesCode(5)`, `EfiRuntimeServicesData(6)`,
  `EfiConventionalMemory(7)`, `EfiACPIReclaimMemory(9)`, `EfiACPIMemoryNVS(10)`;
* `ram_top` = the highest `start + pages * 4096` over all RAM regions;
* `phys_limit = min(ram_top, MAX_IDENTITY_BYTES)` with `MAX_IDENTITY_BYTES = 4 GiB` [decision #14].

A page (below 2 MiB) or a 2 MiB block (above) is mapped **present** iff it overlaps at least one
RAM region, with these exceptions and attributes:

| Rule | Reason |
|---|---|
| page 0 (`0x0..0x1000`) is **never** mapped, even if the firmware calls it RAM | policy: a null dereference must be a clean `#PF` (→ 41), and §8.3 uses it to prove our tables are live |
| non-RAM regions (`EfiReservedMemoryType`, `EfiUnusableMemory`, MMIO / port space, unaccepted, persistent) are **not** mapped | touching them faults loudly instead of silently hitting a device or reserved memory |
| present entries: `RW=1, US=0, PWT=0, PCD=0, NX=0`, and `PS=1` for 2 MiB blocks only | no user mode yet (M4), no MMIO, no W^X split, no caching change |
| a 2 MiB block that *partially* overlaps RAM is mapped **entirely** | granularity trade-off; the non-RAM tail becomes mapped-but-unused, and the frame allocator still refuses to hand any of it out (§5.2) |

`ram_top > MAX_IDENTITY_BYTES` is **not** an error: the kernel maps the first 4 GiB, logs a warning
and continues. 4 GiB is far beyond the test VM (QEMU's default is 128 MiB, and `run-qemu.sh` does
not pass `-m`); the cap exists only because the table storage and the frame bitmap are static
(§3.5). Removing the cap later means placing both in memory taken from the frame allocator itself.

> ⚠️ **MMIO is not mapped by M2.** The kernel drives no MMIO device: serial is port I/O (`0x3F8`),
> and the local APIC, HPET and framebuffer are untouched. The RSDP is only *forwarded* — M2 does not
> read ACPI tables, which is what keeps this restriction harmless today. Any later milestone that
> touches ACPI tables, the local APIC or a framebuffer must add explicit mappings first.

### 3.5 Table storage

```
MAX_IDENTITY_BYTES = 4 GiB
tables = PML4 (1 page) + PDPT (1 page) + PT for the low 2 MiB (1 page) + PD (4 pages, 1 GiB each)
       = 7 pages = 28 KiB
```

The storage is a single `#[repr(align(4096))] static` arena in the kernel image's `.bss`
[decision #14]: it needs no allocator, the bootloader already zeroes it, and its physical address
equals its link-time address. The kernel asserts, before use, that the arena is 4 KiB aligned and
lies inside the kernel image span; either failure is a memory-init failure (43). The builder returns
`Err(OutOfArena)` rather than overflowing if a machine ever needs more tables than budgeted.

### 3.6 Building and installing
1. compute `phys_limit` from the memory map (§4);
2. `kernel_memory::paging::build(arena, &map, config)` → `Result<PageTables, BuildError>`, with
   `PageTables { pml4_phys, mapped_bytes, blocks, tables_used }` — pure logic, host-tested;
3. log one line: `paging: 恒等映射 N MiB，块粒度 2 MiB，低端 2 MiB 用 4 KiB 页，CR3=0x…`;
4. `mov cr3, pml4_phys`, with `CR4.PCIDE` **asserted clear** first (with PCIDE set, `CR3` carries a
   PCID in its low bits, so writing a bare table address would corrupt it);
5. **re-validate `BootInfo` after the switch**: it lives at a physical address, so it is reachable
   only if the identity map really covers it;
6. no `invlpg` and no `sfence` are needed (a `CR3` write flushes non-global TLB entries; single
   core, no write-combining memory).

The firmware's tables are abandoned, not freed: they live in `EfiBootServices*` memory, which M2
never hands out (§5.2).

## 4. Parsing the UEFI memory map

`crates/kernel-memory/src/map.rs` — zero dependencies, host-tested, and it keeps the kernel free of
uefi-rs (`kernel_interface.md` §5.3).

```rust
pub struct MemoryMap<'a> { /* bytes, entries, stride, version */ }
pub struct Region { pub kind: MemoryKind, pub base: u64, pub pages: u64 }

impl<'a> MemoryMap<'a> {
    pub fn parse(ptr: u64, entries: u64, stride: u32, version: u32) -> Result<Self, MapError>;
    pub fn iter(&self) -> impl Iterator<Item = Region> + '_;
    pub fn ram_top(&self) -> Option<u64>;
}

pub enum MapError { Empty, StrideTooSmall(u32), AddressOverflow { index: usize } }
```

Rules:

* iterate with `BootInfo.mmap_desc_size` — 48 under OVMF/QEMU 8.2 — **never** with
  `size_of::<EfiMemoryDescriptor>()` = 40; this is the trap documented in
  `kernel_interface.md` §5.3;
* `stride < 40` → `StrideTooSmall`; `entries == 0` → `Empty`; a descriptor whose
  `base + pages * 4096` overflows `u64` → `AddressOverflow`;
* an unknown `type` becomes `MemoryKind::Unknown(u32)` and counts as **not RAM** — a future firmware
  must not be able to make the kernel map something it does not understand;
* the parser reads only the fields at offsets 0, 8 and 24 (type, base, pages); it ignores
  `VirtualStart` and `Attribute` (unused by M2) but never assumes the trailing padding is zero.

A map error is a **self-check failure (39)**, not 43: it means the bootloader↔kernel contract is
broken, exactly like an invalid `BootInfo`. 43 is reserved for memory initialisation failing later.

## 5. Physical frame allocator

`crates/kernel-memory/src/frame.rs`. Granule: **4 KiB** [decision #16].

### 5.1 State

```rust
pub struct FrameAllocator<'a> { /* two bitmaps + managed range + reserved ranges + counters */ }
```

* two `static` bitmaps in `.bss`, **one bit per frame each**: `allocatable` (`1 = this frame may be
  handed out at all`) and `used` (`1 = currently handed out`);
* they are separate because `free` must tell "a frame that never belonged to the allocator" (firmware
  reserved memory, MMIO, non-conventional memory) apart from "a frame that is already free". With a
  single bitmap both are `1`, and `free` would put memory it must never hand out back into the free
  pool;
* with `MAX_IDENTITY_BYTES = 4 GiB` they cost `2 × 4 GiB / 4 KiB / 8 = 256 KiB`;
* bit *i* describes frame `managed_start + i * 4096`.

### 5.2 Which frames are free

`init(bitmap, &map, reserved, config)` starts with `allocatable = 0` for every frame (nothing may be
handed out) and `used = 1` (nothing is free), then marks a frame allocatable and free only when all of
the following hold:

1. the frame lies completely inside one `EfiConventionalMemory` region — **only** conventional
   memory is handed out in M2 [decision #15], even though `kernel_interface.md` §6.2 says
   boot-services memory becomes free after handoff. Deliberate: reusing firmware memory is a change
   that deserves its own measurement, and M2 does not need the extra memory;
2. the frame is at or above `MIN_FREE_ADDR = 1 MiB` [decision #15]: low memory holds firmware
   structures, and everything below 2 MiB is mapped only as collateral of the low page table;
3. the frame is below `phys_limit`;
4. the frame does not overlap any **explicitly reserved** range: the kernel image
   (`kernel_base..kernel_base + kernel_size`), the kernel stack, the `BootInfo` page, the memory-map
   buffer (`mmap_ptr..mmap_ptr + len * desc_size`), and the page-table arena.

Rule 4 is redundant with rule 1 — all of those ranges are reported as `EfiLoaderData` — and is kept
**on purpose**: the allocator must not depend on the firmware having classified our own allocations
correctly. The self-check cross-checks both the allocator's own reserved list **and** a list the
kernel derives independently from `BootInfo`: neither may contain a free frame.

Measured consequence of rule 1 (QEMU 8.2 + OVMF, default 128 MiB): the managed window is 127 MiB
(32512 frames) but only ~78 MiB (20027 frames) is `EfiConventionalMemory`; the rest is
`BootServices*`/`RuntimeServices*`/ACPI memory that M2 deliberately does not reuse yet.

### 5.3 API contract

```rust
pub fn init(bitmap: &mut [u8], map: &MemoryMap, reserved: &[Range<u64>], cfg: Config)
    -> Result<Self, InitError>;                       // InitError::BitmapTooSmall
pub fn alloc(&mut self) -> Option<PhysFrame>;         // lowest free frame
pub fn alloc_contiguous(&mut self, count: usize) -> Option<PhysRange>;
pub fn free(&mut self, frame: PhysFrame) -> Result<(), FreeError>;
                                                      // AlreadyFree | NotManaged | Reserved
pub fn stats(&self) -> FrameStats;                    // managed / free
pub fn reserved_ranges(&self) -> &[ReservedRange];
pub fn first_free_in(&self, start: u64, end: u64) -> Option<PhysFrame>;
```

* **lowest-first** allocation: deterministic, which makes host tests and the QEMU self-check exact.
  ASLR and fragmentation-aware policies are explicitly out of scope [decision #17];
* `alloc_contiguous` exists because the heap is a single contiguous byte range (§6.1); it first-fits
  `count` consecutive free bits and reports "not enough contiguous memory" instead of returning a
  fragmented region;
* `free` distinguishes `NotManaged` (outside the managed window, or never allocatable),
  `Reserved` (inside an explicitly reserved range) and `AlreadyFree` (double free), so the self-check
  can assert all three. M2 has **no production caller** of `free` (the heap never shrinks); it exists
  for the self-check and for M3, and is covered by host tests;
* `reserved_ranges` and `first_free_in` exist for the self-check's cross-check; they are read-only
  and cannot disturb the free set;
* frame 0 is never managed (it is below `MIN_FREE_ADDR`), so `alloc` can never return it.

### 5.4 Concurrency
None inside M2: after handoff interrupts are disabled and there is one core, so `FrameAllocator` is a
plain `static` behind an `UnsafeCell` with `Sync` — the same pattern M1 uses for the IDT — plus the
documented precondition "wrap it in a lock before enabling interrupts" [decision #20].

> ✅ **M3 has now carried that precondition out**: `FrameAllocator` sits behind an interrupt-safe
> `SpinLock` and every call goes through it (see
> [threads_and_scheduling.md](threads_and_scheduling.md) §5). Decision #20 is therefore satisfied,
> not repealed.

## 6. Kernel heap

### 6.1 Region
* `HEAP_SIZE = 1 MiB` (256 contiguous frames), fixed for M2, taken from the frame allocator at init
  [decision #18];
* contiguous because the heap is a single byte range; a failed `alloc_contiguous` is a memory-init
  failure (43) with a specific message;
* **no growth in M2**: growing the heap needs either a region list under the allocator or a way to
  extend the global allocator's arena, which is a design of its own. M2 documents the limitation and
  its failure mode (allocation failure → panic → 41);
* the heap region is identity-mapped like everything else, and the kernel refers to it by physical
  address — an acceptable M2 simplification, revisited when a higher-half layout arrives.

### 6.2 Algorithm
A hand-written **first-fit free list** whose headers live inside the free blocks [decision #18], in
`crates/kernel-memory/src/heap.rs`, operating on a caller-provided `&mut [u8]` arena so the whole
algorithm is host-tested:

* each block carries a 24-byte header: `magic` (u32), padding, `size` (total block size), `next`
  (offset of the next free block); the magic tells a free block from an allocated one, which is what
  makes double frees and bogus pointers detectable;
* `MIN_BLOCK = MIN_PAYLOAD(16) + HEADER_SIZE(24) = 40` bytes — the smallest block the allocator will
  *split off*; a request whose tail is smaller than that absorbs the tail, so an **allocated** block
  can be as small as its header, and a **free** block smaller than `MIN_BLOCK` may exist until a
  neighbour is freed and coalesces with it. Both cases are covered by host tests;
* `alloc(size, align)`: first fit. The header always sits immediately before the payload — either the
  padding before the payload is empty, or it is large enough to become a free block of its own
  (small padding is rounded up to `MIN_BLOCK`). That is what lets `dealloc` recover the header from
  the pointer alone;
* `dealloc(offset)`: return the block to the list **in address order and coalesce** with both
  neighbours;
* alignment: supports `layout.align()` up to `PAGE_SIZE`; the returned payload satisfies both the
  requested alignment and the 8-byte header alignment;
* `Layout.size == 0` is rejected per `GlobalAlloc`'s contract, as are unsupported alignments;
* failure returns null; `alloc::alloc::handle_alloc_error` (default handler, stable since Rust 1.68)
  panics with "memory allocation of N bytes failed", and the M1 panic handler turns that into exit
  code 41 — **no unstable feature and no `#[alloc_error_handler]` attribute are needed**.

### 6.3 Wiring it in
The kernel binary owns the global allocator, so `Box`, `Vec`, `String`, `BTreeMap` and `format!`
work inside the kernel:

```rust
#[global_allocator]
static KERNEL_HEAP: KernelHeap = KernelHeap::new();   // GlobalAlloc → heap::FreeList
```

The consequences are honest in both directions: the kernel gains dynamic data structures, and it
gains the ability to leak. The self-check therefore asserts that the heap returns to a single free
block when it finishes (§6.4 step 5).

### 6.4 Self-check (the part that proves M2 works)
Runs at the end of `kernel_main`, before the `self-check OK` line, and prints the failing step, the
invariant and the address involved:

1. allocate `N = 64` blocks of varying sizes (16, 33, 64, 129, 256, 1025, 4096, 7 bytes) and write a
   pattern derived from the block index into **every byte** of each block;
2. assert that all blocks are pairwise disjoint and inside the heap region — each of these is a real
   check, not a tautology;
3. read every byte back and compare: this proves the frames are actually mapped and writable, not
   merely that the allocator's arithmetic works;
4. free every other block, allocate `N/2` replacements of the same sizes, and verify that the
   replacements are disjoint from the still-live blocks (address-set disjointness, not an assumed
   address) and that the live blocks are untouched;
5. free everything and assert the heap is back to exactly **one** free block whose size equals the
   whole heap minus one header (1 MiB − 24 = 1048552 bytes) — i.e. coalescing worked — and that two
   allocations of twice the heap size both fail cleanly (null, no wrap, no panic);
6. frame-allocator checks: two fresh allocations must be **different frames** (the one check that
   catches double allocation); freeing and re-allocating must return the same lowest frame; a double
   free must be rejected; no free frame may fall inside a reserved range — checked both against the
   allocator's own list and against a list the kernel derives independently from `BootInfo`; and
   `FrameAllocator::stats().free` must have dropped by exactly `HEAP_SIZE / 4096 = 256` since init;
7. print `heap: 0x…..0x… (1 MiB), 自检 OK`.

Any failure prints the step, the invariant and the address, then exits **43**.

## 7. Exit codes and fault injection

### 7.1 New code

| Code | Meaning | Reported by | Status |
|---|---|---|---|
| **43** | **Kernel memory initialisation failed** — page tables could not be built or installed, the frame allocator could not be initialised, or the heap self-check failed | kernel | new in M2 |

39 keeps its meaning ("the kernel concluded it cannot run": invalid `BootInfo`, unparsable memory
map, no RSDP). The distinction matters: 39 is a broken handoff, 43 is a broken memory subsystem, and
they have different owners.

### 7.2 Fault injection (test only)
Two features, both carrying the standing note "production builds never enable":

* `inject-memory-fault` (new): the frame allocator hands out a frame **without marking it used**
  (crate feature `inject-double-alloc`, forwarded by the kernel feature of the same name), so §6.4
  step 6's "two fresh allocations must be different frames" check must fail → **43**. This is the
  failure mode worth injecting: two callers silently getting the same memory is the most dangerous
  bug a frame allocator can have, and it is invisible until something corrupts;
* `inject-null-deref` (new): after the page tables are installed, deliberately reads address `0`,
  unmapped by policy (§3.4) → `#PF` (vector 14) → the M1 exception handler → **41**. This is the
  only test that proves the *kernel's* tables are active rather than the firmware's.

`inject-fault` (M1, `ud2` → 41) now runs **after** the page tables are installed, so the existing M1
acceptance case additionally proves that exception handling still works under our own tables.

## 8. Delivery plan, deliverables and acceptance criteria

### 8.1 Split

| Part | Content | Why split |
|---|---|---|
| **M2a** | `crates/kernel-memory` with `map.rs` + `paging.rs` (host-tested, pure); the kernel builds and installs its own tables and re-validates `BootInfo` afterwards; exit code 43; `MAX_KERNEL_PAGES` raised in the bootloader | a page-table bug and a heap bug have very different diagnostics; the split keeps each PR's failure surface small |
| **M2b** | `frame.rs` + `heap.rs` (host-tested); `#[global_allocator]`; the §6.4 self-check; the two injection features; new smoke-test cases | builds on a mapping M2a has already proved |

`MAX_KERNEL_PAGES` had to grow twice: 64 → 128 pages in M2a (page-table arena, 28 KiB) and
128 → 256 pages (1 MiB) in M2b (the two frame bitmaps, 256 KiB, plus `alloc` and formatting code put
the image at 104 pages). Forgetting this surfaces as the loader's `TooManyPages` error (exit 35) —
which is exactly what the acceptance criterion about the image span guards against.

### 8.2 Deliverables

| # | Deliverable | Notes |
|---|---|---|
| 1 | `crates/kernel-memory` | zero dependencies, `no_std`; host unit tests for map + paging (M2a) and frames + heap (M2b) |
| 2 | Kernel page tables | built from the memory map, installed via `CR3`, `BootInfo` re-validated after the switch |
| 3 | Frame allocator | bitmap over conventional memory, lowest-first, contiguous allocation, self-check cross-checks |
| 4 | Kernel heap | 1 MiB first-fit free list, `#[global_allocator]`, coalescing verified by the self-check |
| 5 | `crates/kernel/src/memory.rs` | the kernel-side glue: statics, arena, init sequence, self-check |
| 6 | Exit code 43 + two injection features | §7 |
| 7 | `tests/smoke.sh` | new M2 cases (§8.3 / §8.4) |
| 8 | Docs | this document, `kernel_interface.md` §7.2 and §9, `README` / `VISION` capability tables — all bilingual |

### 8.3 Acceptance criteria — M2a (**met**, PR #16)
- [x] the kernel logs `paging: 恒等映射 …` with a mapped size ≥ 64 MiB and a 2 MiB block granularity;
- [x] the kernel re-validates `BootInfo` after installing the tables (log line), i.e. the identity
      map demonstrably covers the handoff structures;
- [x] `tests/smoke.sh` still asserts 37 for the normal case and 35 / 35 / 39 / 41 for the M1
      negative cases;
- [x] `inject-null-deref` exits **41** and the log names vector 14 (`#PF`) — direct proof that the
      kernel's tables, not the firmware's, are active;
- [x] `python3 tests/check_kernel_elf.py` passes and additionally asserts that the page-aligned load
      span fits within `MAX_KERNEL_PAGES`;
- [x] `cargo test -p kernel-memory` passes (map + paging tests).

### 8.4 Acceptance criteria — M2b (**met**, PR #17)
- [x] the log contains a frame count with `free > 0` and a heap line for a 1 MiB region;
- [x] all seven self-check steps pass on real QEMU + OVMF, including the byte-for-byte read-back of
      64 blocks, the double-free detection and the coalescing assertion;
- [x] `inject-memory-fault` exits **43**, while the normal case still exits **37**;
- [x] `inject-fault` (M1) still exits **41**, now under the kernel's own page tables;
- [x] the free-frame count dropped by exactly `HEAP_SIZE / 4096` after init;
- [x] `cargo test -p kernel-memory` covers allocate / free / reuse / coalesce / alignment /
      exhaustion / double-free;
- [x] no existing smoke assertion regresses, and fmt/clippy stay clean in every configuration already
      used by CI.

Measured on QEMU 8.2.2 + OVMF (default 128 MiB) with 35/35 smoke assertions green:

```
frames: 管理 32512 帧（127 MiB），堆取走后空闲 20027 帧
heap: 0x168000..0x268000（1024 KiB，占用 256 个连续页帧）
heap: 自检 OK（全部释放后空闲 1048552 字节 / 1 个块）
```

### 8.5 Known pitfalls, most likely first

| # | Pitfall | Mitigation |
|---|---|---|
| 1 | Writing page-table entries through the firmware's mapping and reading them back after `CR3` — same physical address, but easy to muddle | the builder is pure and host-tested; the kernel only copies finished tables |
| 2 | Iterating the memory map with 40-byte strides | §4, plus a host test with `stride = 48` |
| 3 | `MAX_KERNEL_PAGES` smaller than the new image | §8.1; the acceptance criterion checks the span |
| 4 | `CR4.PCIDE` set, so a bare `CR3` write corrupts the address | asserted before the write (§3.6) |
| 5 | Assuming the kernel image / stack / `BootInfo` are never in conventional memory | explicit reserved ranges plus the self-check cross-check (§5.2) |
| 6 | Freeing a block with a different `Layout` than it was allocated with (`GlobalAlloc` permits this) | the header lives before the payload; a host test allocates with one alignment and frees with another |
| 7 | `#[global_allocator]` + `alloc` missing from the target's sysroot | verified by building in M2b before anything else |
| 8 | A heap self-check that passes while the frames are unmapped (arithmetic-only) | step 3 writes and reads back every byte of every block |

## 9. Decision record

| # | Decision | Outcome | Rationale | Reversibility |
|---|---|---|---|---|
| 11 | Own page tables in M2, instead of trusting the firmware's identity map | 4-level, built from the memory map, installed via `CR3` | from M2 on the mapping is load-bearing (heap frames); `kernel_interface.md` §4/§6.2 already warn it is not guaranteed; also a prerequisite for M4 | medium (the builder is pure and replaceable) |
| 12 | 2 MiB blocks above the first 2 MiB | 4 KiB pages only for `0x0..0x20_0000` | ~1000× less table storage for the same coverage, which keeps the tables static | high |
| 13 | Non-RAM addresses are not mapped | any access faults loudly (→ 41) | silence is the worst failure mode for a memory subsystem | high |
| 14 | Static `.bss` arena for tables, static bitmap for frames, 4 GiB cap | 28 KiB + 128 KiB | no allocator needed to bootstrap; a static cap is honest and testable; the cap goes away when both move into allocated memory | medium |
| 15 | Only `EfiConventionalMemory` at or above 1 MiB is handed out | conservative free set | boot-services reuse deserves its own measurement; 1 MiB keeps firmware structures and the collateral low-memory mapping out of the allocator's reach | high |
| 16 | 4 KiB frame granule | uniform | the only granule that can back a 4 KiB page-table entry; large-page-aware allocation is a later optimisation | high |
| 17 | Lowest-first, contiguous-capable allocation | deterministic | exact host and QEMU assertions; no ASLR or fragmentation policy in M2 | high |
| 18 | Hand-written first-fit heap, 1 MiB, no growth | host-tested in `crates/kernel-memory` | keeps the kernel dependency-free (AGENTS.md rule 2) and keeps the algorithm in a crate that can really be unit-tested | high |
| 19 | New exit code 43 for memory-init failure | distinct from 39 | a broken memory subsystem and a broken handoff have different owners and different fixes | medium (released meanings are frozen) |
| 20 | No locking in M2 | documented as a precondition | single core, interrupts disabled; an untested lock would be worse than a written precondition | high |
| 21 | The null page is never mapped | policy, regardless of the firmware's type | turns null dereferences into `#PF` (→ 41) and gives M2a a test that proves our tables are live | high |
| 22 | Two frame bitmaps (`allocatable` + `used`) instead of one | 256 KiB instead of 128 KiB | with one bitmap, `free` cannot tell "never allocatable" (MMIO, reserved) from "already free", so it would put memory it must never hand out into the free pool | medium |
| 23 | The injection for 43 is "hand out the same frame twice" | crate feature `inject-double-alloc` | double allocation is the most dangerous frame-allocator bug and the one the self-check exists to catch; corrupting the reserved list would not have been observable | high |

## 10. Interface change process
1. `crates/kernel-memory`'s public items are an interface between the kernel and its own logic;
   changes to them must come with host tests.
2. Anything that changes the **bootloader↔kernel** contract (`BootInfo`, entry ABI, exit codes)
   follows `kernel_interface.md` §11.
3. Bilingual docs: every change must update both this file and
   [memory_subsystem_CN.md](memory_subsystem_CN.md) (AGENTS.md rule 4).
4. Implementation status: after each part, update the status column in §8 and the M2 row of
   `kernel_interface.md` §9.
