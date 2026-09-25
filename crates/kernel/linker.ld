/* ParanukOS 内核链接脚本（Milestone 0）
 *
 * 约定（见 docs/architecture/kernel_interface.md §3）：
 *   - 内核是 ELF64 / ET_EXEC，按固定地址链接，由引导器按 p_paddr 装载；
 *   - .text/.rodata 为可执行段，.data/.bss 为可写段（因此会产生两个 PT_LOAD 段，
 *     引导器必须支持多段）；
 *   - 入口符号为 kernel_main（链接器据此设置 ELF 的 e_entry）。
 */

ENTRY(kernel_main)

SECTIONS
{
    . = 0x100000;

    .text : ALIGN(4K) {
        *(.text .text.*)
        *(.rodata .rodata.*)
    }

    .data : ALIGN(4K) {
        *(.data .data.*)
    }

    .bss : ALIGN(4K) {
        *(.bss .bss.*)
        *(COMMON)
    }

    /DISCARD/ : {
        *(.eh_frame)
        *(.comment)
        *(.note .note.*)
    }
}
