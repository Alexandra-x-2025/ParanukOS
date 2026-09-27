/* ParanukOS 用户态服务链接脚本（user_mode.md §4）
 *
 * 两个要点：
 *  1. **虚拟地址在用户区**（USER_BASE = 4 GiB，内核恒等映射的终点），物理装载地址在低地址；
 *  2. **整段使用同一个 vaddr - paddr 差值**（USER_DELTA），因为内核只拿一个差值就把整个
 *     物理装载区间映射到用户虚拟地址（BootInfo v1 的 user_vaddr，决策 #43）。
 *
 * ELF 头与程序头被并进 text 段（FILEHDR PHDRS）：否则链接器会为它们单独生成一个 PT_LOAD，
 * 而那个段的物理地址会等于虚拟地址（4 GiB），引导器无法按它装载。
 */

ENTRY(_start)

USER_BASE  = 0x100000000;   /* 4 GiB：内核恒等映射的终点 */
USER_TEXT  = 0x100001000;   /* 头部（FILEHDR）+ .text 的起点，比 USER_BASE 高一页 */
USER_DELTA = 0xFE000000;    /* paddr = vaddr - USER_DELTA，于是镜像装载在 0x02000000（32 MiB，RAM 内）*/

PHDRS
{
    text PT_LOAD FILEHDR PHDRS FLAGS(5);   /* R + X */
    data PT_LOAD FLAGS(6);                 /* R + W */
}

SECTIONS
{
    . = USER_TEXT;

    .text : AT(ADDR(.text) - USER_DELTA) ALIGN(4K) {
        *(.text .text.*)
        *(.rodata .rodata.*)
    } :text

    .data : AT(ADDR(.data) - USER_DELTA) ALIGN(4K) {
        *(.data .data.*)
    } :data

    .bss : AT(ADDR(.bss) - USER_DELTA) ALIGN(4K) {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ : {
        *(.eh_frame)
        *(.comment)
        *(.note .note.*)
    }
}
