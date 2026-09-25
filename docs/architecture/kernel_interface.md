# ParanukOS Kernel Interface and Milestone 0

[English] | [中文](kernel_interface_CN.md)

> **Status: interface v0 is settled. Implementation status: not implemented (M0 has not started).**
>
> This document defines **only the interface between the bootloader and the kernel**, plus the
> first verifiable milestone. The microkernel itself (IPC, capability model, scheduling,
> user-space services, Wasm) is **out of scope** here; those belong to the parts marked as
> "direction" in [core_kernel.md](core_kernel.md).

## 1. Scope

### 1.1 What this document defines
* The kernel image format and load rules (§3)
* The entry ABI used to jump from the bootloader into the kernel (§4)
* The `BootInfo` structure shared by both sides (§5)
* Handoff semantics and what stops working after handoff (§6)
* Observable channels and the exit-code convention (§7)
* Milestone 0 deliverables and falsifiable acceptance criteria (§8)
* The interface change process (§11)

### 1.2 What this document does not define
Paging and virtual memory, user mode, IPC, the capability model, scheduling, SMP, Wasm/WASI,
file systems, ACPICA table parsing, a kernel heap, graphics output, and interrupt-handling
infrastructure.

### 1.3 Terminology

| Term | Meaning |
|---|---|
| ESP | EFI System Partition — the FAT partition from which UEFI firmware loads `\EFI\BOOT\BOOTX64.EFI` |
| `PT_LOAD` | An ELF program header segment that must be loaded into memory |
| `p_paddr` | The **physical** load address of a segment (per the ELF specification) |
| `p_vaddr` | The **virtual** load address of a segment |
| `ET_EXEC` | The ELF type for a non-PIE executable; a PIE is `ET_DYN` |
| `LOADER_DATA` | UEFI memory type for memory owned by the bootloader; the kernel allocator must treat it as in use |

## 2. Current baseline

### 2.1 Implemented

| Capability | Where |
|---|---|
| Buildable, bootable UEFI application (PE32+ / subsystem=10) | `src/main.rs`, `.cargo/config.toml` |
| Locating the ESP it was started from | `boot::get_image_file_system` (`src/bootloader/fs_loader.rs`) |
| Validating the kernel image as ELF64 / little-endian / x86-64 / `ET_EXEC` and extracting the entry point | `crates/kernel-image` |
| Copying the image into memory, page by page (**arbitrary address**) | `boot::allocate_pages(AnyPages, …)` |
| End-to-end smoke test, PE structure check, unit tests, CI | `tests/`, `.github/workflows/ci.yml` |
| Kernel IDT (32 CPU exceptions), exception/panic diagnostics, serial logging | `crates/kernel/src/idt.rs`, `logging.rs` |
| Exact exit codes (33 success / 35 load failure / 124 timeout → failure) | `--features qemu-exit` + `run-qemu.sh` |

### 2.2 Gaps (what M0 closes)

| Gap | Consequence |
|---|---|
| No `exit_boot_services`, no jump | The kernel never executes |
| The image is copied to an **arbitrary** address | The first jump would crash (code and data are fetched at their link-time addresses) |
| No entry convention and no `BootInfo` | Even if executed, the kernel gets no memory map, no RSDP and no stack |
| No kernel-side code | There is no kernel in this repository yet |

## 3. Kernel image format and load rules

### 3.1 Format requirements
ELF64, little-endian (`ELFDATA2LSB`), `e_machine = EM_X86_64(62)`, `e_type = ET_EXEC(2)`.

### 3.2 Load algorithm
1. Validate the ELF header (reuse `crates/kernel-image`);
2. Read the program header table (`e_phoff` / `e_phnum` / `e_phentsize`; `e_phentsize` must be ≥ 56);
3. For every segment with `p_type == PT_LOAD`:
   1. require `p_memsz > 0` and `p_align` either a power of two or 0/1;
   2. allocate the **page-aligned range** covering the segment (one page at a time, with `AllocateType::Address(page)`): `start = p_paddr & !0xFFF`, `end = align_up(p_paddr + p_memsz)`. **`p_paddr` is not guaranteed to be page-aligned** (the linker only guarantees `p_vaddr ≡ p_offset (mod p_align)`), so allocating at `p_paddr` directly fails; pages already allocated for an earlier segment must be **reused** rather than allocated again;
   3. copy `p_filesz` bytes to `p_paddr`;
   4. zero the range `[p_paddr + p_filesz, p_paddr + p_memsz)` (BSS);
4. Physical ranges of all loaded segments **must not overlap**, and must not cover the
   bootloader itself, `BootInfo`, the kernel stack, or the memory map buffer;
5. Verify that `e_entry` falls inside a `PT_LOAD` segment with `p_flags & PF_X`.

### 3.3 Rejection conditions (always exit with code 35, printing the specific reason)
Invalid ELF header / not `ET_EXEC` / program header table out of bounds / invalid segment type /
overlapping segments / `p_filesz > p_memsz` / address allocation failure /
`e_entry` outside an executable segment.

### 3.4 Why these rules
* **Loading at `p_paddr` is mandatory.** A non-PIE kernel accesses globals, static tables and
  function pointers through **absolute addresses**. Copy it somewhere else and jump, and the
  first access to `.rodata`/`.bss` reads the wrong physical address. Loading at `p_paddr` is the
  only general approach that stays consistent with the linker.
* **Single source of truth.** The load address comes from the kernel's own program headers, not
  from a constant in the bootloader, so the bootloader constant and the kernel linker script
  cannot drift apart.
* **Nothing is given up for the future.** Supporting a higher-half layout later is a smooth
  evolution: load at `p_paddr`, map at `p_vaddr`, enter at `p_vaddr`.
* **Segments are allocated by page-aligned range, not at `p_paddr` directly.** A real kernel image has segments whose `p_paddr` is not page-aligned (the v0 kernel's `.data` segment lands at `0x104B38`), and segment tails routinely share a page with the next segment. Both cases are handled by allocating the page-aligned range and reusing pages that an earlier segment already owns.
* **Accepting only `ET_EXEC` is a deliberate simplification.** PIE (`ET_DYN`) would require
  relocation processing (`R_X86_64_RELATIVE`), which M0 does not do. It is a trade-off, not an
  oversight.

## 4. Entry ABI v0

| Item | Convention |
|---|---|
| Entry address | The ELF `e_entry` (looked up **without** symbol names, so stripped images still work) |
| Argument | `rdi = physical address of BootInfo` (System V AMD64 calling convention) |
| Stack | The bootloader allocates 64 KiB (16 pages) and sets `rsp` to the top **minus 8 bytes** (see §4.1) |
| Interrupts | Disabled (`cli`) |
| Page tables | Whatever identity mapping the firmware left behind — the kernel **must not rely on it** |
| Return value | The entry point must not return (`-> !`); returning is a protocol violation |
| `BootInfo` location | One page allocated and filled by the bootloader (see §5.4) |

### 4.1 Exact stack alignment requirement
System V AMD64 requires `(rsp + 8) % 16 == 0` at function entry, because `call` pushes an 8-byte
return address. We use `jmp` rather than `call`, so we must satisfy that condition ourselves:
**the bootloader sets `rsp` such that `rsp % 16 == 8`.**

Consequence of getting it wrong: any SSE instruction in the kernel (`movaps`, …) raises `#GP`,
and it happens before the first meaningful kernel instruction — a nearly undebuggable failure.
The kernel is also allowed to re-align the stack in its own entry as a second line of defence.

### 4.2 Corollary: the kernel entry can be plain Rust
Because the stack is already valid and correctly aligned, the first version of the kernel can be:

```rust
#[unsafe(no_mangle)]
pub extern "C" fn kernel_main(boot_info: &BootInfo) -> ! { /* ... */ }
```

**No assembly stub or naked function is required**, which removes the most error-prone part of M0.

## 5. `BootInfo` v0

### 5.1 Definition
It lives in its own crate (`crates/boot-info`), depended on by both the bootloader and the kernel,
so the two sides cannot silently drift apart. Zero dependencies, `#![no_std]`, with a `validate()`
that is unit-tested on the host.

```rust
#[repr(C)]
pub struct BootInfo {
    pub magic: u64,          // 0x5061_7261_6E75_6B01 ("Paranuk" + interface version)
    pub version: u32,        // v0 = 0
    pub size: u32,           // total size in bytes (v0 = 88; pinned by a unit test)

    pub mmap_ptr: u64,       // physical address of the EFI_MEMORY_DESCRIPTOR array
    pub mmap_len: u64,       // number of descriptors
    pub mmap_desc_size: u32, // descriptor stride (see §5.3)
    pub mmap_desc_ver: u32,  // UEFI descriptor version

    pub rsdp: u64,           // physical address of the ACPI RSDP; 0 = not found

    pub kernel_base: u64,    // physical base address the kernel image was loaded at
    pub kernel_size: u64,    // kernel image size in bytes

    pub stack_top: u64,      // top of the kernel stack (= rsp at jump time + 8)
    pub stack_size: u64,     // kernel stack size in bytes

    pub exit_port: u32,      // isa-debug-exit port; 0 = disabled (see §7.3)
    pub _reserved: u32,      // keeps 8-byte alignment

    // New fields are only ever appended here, with version+1 and a larger size
}
```

### 5.2 Field semantics
* The memory `mmap_ptr` points to has type `LOADER_DATA` and **must be preserved by the kernel**
  (see §6.3); `mmap_len = 0` means it is unavailable.
* The bootloader obtains `rsdp` from the UEFI configuration table **before** `exit_boot_services`:
  prefer `ConfigTableEntry::ACPI2_GUID`, fall back to `ACPI_GUID`.
* `stack_top` is the high end of the stack and `stack_size` is the total size; that memory is also
  `LOADER_DATA`.

### 5.3 Memory map descriptor layout
The kernel **must not depend on uefi-rs**; it parses the array itself. On x86-64 each descriptor is
**40 bytes**:

| Offset | Field | Type | Notes |
|---|---|---|---|
| 0 | `Type` | u32 | `EFI_MEMORY_TYPE` (1=LoaderCode, 7=Conventional, …) |
| 4 | *padding* | u32 | alignment padding |
| 8 | `PhysicalStart` | u64 | physical start address |
| 16 | `VirtualStart` | u64 | always 0 before any mapping is built |
| 24 | `NumberOfPages` | u64 | **page count (4 KiB pages)** |
| 32 | `Attribute` | u64 | memory attribute bits |

> ⚠️ Iterate using `mmap_desc_size`, **not** `sizeof::<MemoryDescriptor>()`: UEFI allows firmware
> to return descriptors larger than the standard struct — OVMF under QEMU 8.2 actually reports
> `desc_size = 48`, so iterating with the standard 40 bytes would mis-parse every entry.

### 5.4 Where `BootInfo` itself lives
The bootloader allocates **one page** (`MemoryType::LOADER_DATA`), writes `BootInfo` at the start of
it, and puts that page's **physical address** in `rdi`. This way:

* the structure cannot be invalidated by the bootloader's stack or pool allocations;
* the kernel can hold the pointer for as long as it likes;
* its memory type is `LOADER_DATA`, so the kernel's allocator will not hand it out.

### 5.5 Compatibility rules
* The kernel reads **only fields within `size`**;
* new fields are appended, with `version` incremented and `size` grown;
* removing or changing an existing field or its meaning is a breaking change and requires a major
  bump (the version field inside `magic`);
* on a `magic` or `version` mismatch the kernel must print the reason and exit with **code 39** —
  it must never continue blindly.

## 6. Handoff semantics

### 6.1 Handoff sequence (bootloader side; the order is not interchangeable)
1. Parse the ELF and load the kernel per §3.2;
2. allocate the kernel stack (`LOADER_DATA`, 64 KiB);
3. collect the memory map (`boot::memory_map`) and keep its buffer alive;
4. read the RSDP (`system::with_config_table`);
5. allocate one page for `BootInfo` (do not fill it in yet);
6. **print the last line** (the firmware console is unusable from here on);
7. `unsafe { boot::exit_boot_services(Some(MemoryType::LOADER_DATA)) }` — its **return value is the authoritative memory map** (its map key is the one used to exit);
8. **disable interrupts (`cli`) immediately** — after the exit the firmware's interrupt vectors and handlers are no longer valid, so a leftover hardware interrupt would jump into a dead firmware ISR and can corrupt memory;
9. fill in `BootInfo`, including the memory-map pointer / len / desc_size / desc_ver taken from that returned map — pure memory writes, still safe after the exit;
10. set `rsp = stack_top - 8` (satisfying §4.1);
11. `rdi = physical address of BootInfo`;
12. `jmp e_entry`.

> The memory map comes from the **return value of `exit_boot_services`**, not from an earlier
> `memory_map()` call: allocating the kernel stack and the `BootInfo` page invalidates any earlier
> map key, so only the map returned by the exit call is authoritative. Filling `BootInfo` after the
> exit is safe because it involves no boot service.

### 6.2 What stops working after handoff

| Resource | After handoff |
|---|---|
| Boot Services | ❌ All gone, including the `uefi` crate's logger, `println!`, `system::with_stdout` (it asserts that boot services are still active) and every `allocate_*` |
| Pool allocator | ❌ Unusable. Do not allocate again; types that free on `Drop` must be released or `mem::forget`ed beforehand |
| `BOOT_SERVICES_CODE` / `BOOT_SERVICES_DATA` memory | Becomes **free memory** — including the bootloader's own image, so **the bootloader must never be called back into** |
| Runtime Services | ✅ Still available (`ResetSystem`, `GetTime`, variable services) |
| Firmware GDT / IDT | ❌ No longer valid. Until the kernel installs its own IDT, any exception or interrupt can cause a triple-fault reset |
| Page tables | ⚠️ Usually still an identity mapping, but **not guaranteed** |

> ⚠️ **Observed during M1 development:** with the kernel grew and the timing shifted, `BootInfo`'s
> first word was intermittently overwritten (4 bytes) before the kernel validated it. The window
> between the exit call and `cli` is the prime suspect, which is why step 8 above disables
> interrupts immediately. The corruption could not be reproduced deterministically, so this is a
> hardening, not a proven fix; the kernel's `BootInfo` validation is what turns it into a clear
> error (39) instead of silent misbehaviour.

### 6.3 Memory ownership after handoff
The kernel's physical allocator **must treat `LOADER_DATA` as in use**. At minimum that covers: the
kernel image (pages from §3.2), the kernel stack, the memory map buffer, and the `BootInfo` page.

### 6.4 Failure semantics (important)
Failure of `uefi::boot::exit_boot_services` is **unrecoverable**: the uefi-rs implementation
**resets the machine** instead of returning an error. There is therefore no "exit failed → report
gracefully with an exit code" path. Tests must treat "the machine was reset" as a **failure** and
must not accept "QEMU exited normally" as success — which is exactly why the smoke test asserts
exact exit codes.

## 7. Observable channels and exit codes

### 7.1 Serial port (the kernel's first output channel)
COM1 = `0x3F8`, **polled** writes (no interrupts). Rationale: the firmware console is unusable after
`exit_boot_services`, while the serial port keeps working — and QEMU's `-nographic` feeds it
straight into the test's log file.

### 7.2 Exit-code convention
QEMU's `isa-debug-exit` exit code is `(value << 1) | 1`.

| Code | Meaning | Reported by | Status |
|---|---|---|---|
| 0 | Human quit (`Ctrl+A` then `X`), or firmware finished normally | — | implemented |
| **33** | Bootloader finished loading — **reserved since M0**: on success the bootloader jumps into the kernel and the *kernel* reports 37 | bootloader | reserved |
| **35** | Kernel image load failure | bootloader | implemented (same) |
| **37** | **Kernel self-check passed** (`BootInfo` valid, memory map usable, RSDP present) | **kernel** | implemented (M0) |
| **39** | **Kernel self-check failed** — the kernel concluded it cannot run (magic/version mismatch, missing memory map, no RSDP) | **kernel** | implemented (M0) |
| **41** | **Kernel fault or panic** — an unhandled CPU exception (`#UD`, `#GP`, `#PF`, …) or a `panic!` | **kernel** | implemented (M1) |
| **43** | **Kernel memory initialisation failed** — page tables, frame allocator or heap; see [memory_subsystem.md](memory_subsystem.md) §7.1 | **kernel** | reserved (M2) |
| 124 | Timed out without exiting (treated as a hang) | — | implemented (fails) |

**33 and 37 must stay distinct**: otherwise the test cannot tell "the bootloader loaded and stopped"
from "the kernel actually ran" — and the latter is precisely what M0 has to prove. Without the
`qemu-exit` feature the bootloader prints a message and spins (interactive mode).

### 7.3 `exit_port` semantics
* `0`: debug exit disabled. After its self-check the kernel should enter a `hlt` loop
  (interactive/normal mode).
* non-zero: the value is the `isa-debug-exit` I/O port. The kernel writes `0x12` when its self-check
  passes (→ 37) and `0x13` on failure (→ 39).
* The bootloader must pass `-device isa-debug-exit,iobase=0xf4,iosize=0x04` on the QEMU command line
  (QEMU 8.2 defaults `iobase` to `0x501`) and place `0xf4` in `exit_port` in test mode.

## 8. Milestone 0: Hello from kernel

### 8.1 Goal
The bootloader loads the kernel per §3 → hands off per §6.1 → the **kernel** prints one line on the
serial port summarising `BootInfo` → exits with code **37**.

### 8.2 Non-goals
Paging/virtual memory, user mode, IPC, the capability model, scheduling, SMP, Wasm/WASI, file
systems, ACPICA table parsing (the RSDP is merely forwarded), a kernel heap, graphics output,
interrupt handling.

### 8.3 Deliverables

| # | Deliverable | Notes |
|---|---|---|
| 1 | `crates/boot-info` | `BootInfo` v0 + `validate()` + host unit tests (zero dependencies) |
| 2 | `crates/kernel` | `#![no_std]`, `x86_64-unknown-none`, fixed link address (tentatively `0x100000`), one or more `PT_LOAD` segments (the bootloader must support multiple); entry validates `BootInfo` → serial output → writes `exit_port` |
| 3 | `src/bootloader/` | Program header parsing, loading at `p_paddr` + BSS zeroing, stack allocation, `BootInfo` write, `exit_boot_services`, jump |
| 4 | `run-qemu.sh` | Place both `BOOTX64.EFI` and `KERNEL.ELF` in the ESP |
| 5 | `tests/smoke.sh` | New M0 case: assert exit code 37 and assert the kernel's self-check line appears |

### 8.4 Acceptance criteria (each machine-checkable)
- [ ] The ELF produced by `cargo build -p kernel` has `e_type=2`, `e_machine=62`, and `e_entry`
      inside an executable `PT_LOAD` segment;
- [ ] the bootloader artifact is still PE32+ / subsystem=10;
- [ ] the smoke test asserts exit code **37** (kernel self-check passed), **not** 33;
- [ ] the serial log contains kernel output, with a memory-map entry count **> 0** and `rsdp != 0`
      (expected under QEMU+OVMF);
- [ ] **negative case 1**: replacing `KERNEL.ELF` with a non-ELF file → exit code **35**;
- [ ] **negative case 2**: injecting a wrong `BootInfo.magic` → kernel self-check fails, exit code **39**;
- [ ] the existing 9 assertions do not regress.

### 8.5 Known pitfalls, most likely first

| # | Pitfall | Mitigation |
|---|---|---|
| 1 | Using boot services after `exit_boot_services` (usually "one more print") | finish all printing before step 6 of §6.1 |
| 2 | The kernel is a PIE (`ET_DYN`) | build the kernel with `-C relocation-model=static`; the loader rejects it per §3.3 |
| 3 | Load address ≠ link address | load strictly at `p_paddr` (§3.2) |
| 4 | Stack alignment other than `rsp % 16 == 8` | bootloader sets it per §4.1; the kernel entry may re-align |
| 5 | BSS not zeroed | zero it per step 3.4 of §3.2 |
| 6 | Firmware IDT gone after handoff → every bug becomes a triple-fault reset | installing a minimal IDT is the first task of M1 (§9) |
| 7 | Wrong `isa-debug-exit` port | `-device isa-debug-exit,iobase=0xf4,iosize=0x04` + `exit_port=0xf4` |
| 8 | Iterating the memory map with `sizeof` instead of `desc_size` | follow the warning in §5.3 |

## 9. Later milestones (M3+ are placeholders, **not designed**)

| Milestone | Content | Requires |
|---|---|---|
| M1 | Minimal IDT + panic handler + kernel serial logging — **done** | M0 |
| M2 | **Interface defined:** [memory_subsystem.md](memory_subsystem.md) — kernel page tables + physical frame allocator (driven by the memory map from M0, treating `LOADER_DATA` as in use) + kernel heap; delivered as M2a/M2b | M1 |
| M3 | Single-core kernel thread/scheduling skeleton | M2 |
| M4 | First user-space service (minimal privilege switch; no IPC semantics yet) | M3 |
| M5 | IPC message format + capability token semantics (**only now**, and constrained by real user-space processes) | M4 |

> This table expresses **ordering dependencies only**; it is not a design. Each item needs its own
> interface document before work starts.

## 10. Decision record

| # | Decision | Outcome | Rationale | Reversibility |
|---|---|---|---|---|
| 1 | Kernel load method | Load segment by segment at `p_paddr` | Consistent with the linker; avoids two sources of truth for the address; does not block a higher-half layout later | medium (changing it touches both sides) |
| 2 | Image format | ELF64 `ET_EXEC` | Produced natively by the toolchain, keeps symbols for gdb, half the validation already exists; a flat format saves little and loses debuggability | high (swappable, but requires a new loader and tooling) |
| 3 | Entry passing | `e_entry` + `rdi=&BootInfo` | No symbol-table dependency (works when stripped); SysV lets the kernel entry be plain Rust | low (it is an ABI) |
| 4 | Who provides the stack | The bootloader allocates it and sets `rsp` | The kernel has a usable stack before its first instruction; combined with #3 it removes the assembly stub | low (it is an ABI) |
| 5 | Where `BootInfo` lives | A shared crate | Two hand-written `repr(C)` copies inevitably drift, and the failure mode is a mysterious crash from misread memory | high |
| 6 | Kernel output channel | Polled serial on COM1 | The firmware console is gone after handoff; serial is captured by `-nographic` and can be asserted on | high (a framebuffer can be added later) |
| 7 | How "the kernel ran" is asserted | The kernel writes `exit_port` (37/39) | Reusing 33 would let a bootloader that loads but never jumps pass, destroying the test's discriminating power | medium (once released, exit-code meanings should not be reused) |
| 8 | Separate `kernel` crate | Yes | Different target (`x86_64-unknown-none`), and the kernel must not link `uefi` | high |
| 9 | When to install a minimal IDT | First task of M1 (not M0) | M0's kernel only prints a self-check, so the fault surface is small; getting a closed loop first is worth more | high |
| 10 | Documentation language | Maintain the `_CN` counterpart | `AGENTS.md` rule 4 | — |

## 11. Interface change process
1. **`BootInfo`**: only "append a field + `version`+1 + larger `size`" is allowed; breaking changes
   require a major bump.
2. **Entry ABI** (§4): any change must update `crates/boot-info`, the bootloader and the kernel
   together, and must be reflected here and in the interface index of
   [core_kernel.md](core_kernel.md).
3. **Exit codes** (§7.2): new codes must not reuse existing meanings; released meanings may only be
   marked deprecated.
4. **Bilingual docs**: every change must update both `kernel_interface.md` and `kernel_interface_CN.md`
   (`AGENTS.md` rule 4).
5. **Implementation status**: after each milestone, update the status columns in §2.1 and §7.2 so the
   documentation cannot drift from the code again.
