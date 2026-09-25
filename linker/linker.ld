MEMORY
{
    /* 定义内核加载的起始地址 */
    _kernel_start = 0x100000; /* 1MB 处开始 */
    _kernel_end = _kernel_start + 0x200000; /* 分配 2MB 空间 */

    _text : {
        _stext = .;
        *(.text .text.*)
        *(.rodata .rodata.*)
        _etext = . ;
    } aligns(4K)

    _data : {
        _sdata = .;
        *(.data .data.*)
        _edata = . ;
    } aligns(4K)

    _bss : {
        _sbss = _edata;
        *(.bss .bss.*)
        _ebss = . ;
    } aligns(4K)
}

SECTIONS
{
    .text : {
        _stext = .;
        *(.text .text.*)
        *(.rodata .rodata.*)
        _etext = .;
    } > _kernel_start

    .data : {
        _sdata = .;
        *(.data .data.*)
        _edata = .;
    } > _kernel_start

    .bss : {
        _sbss = _edata;
        *(.bss .bss.*)
        _ebss = .;
    } > _kernel_start
}
