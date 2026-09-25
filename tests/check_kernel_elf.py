#!/usr/bin/env python3
"""校验内核镜像是否为可被引导器按 p_paddr 装载的 ELF64。

对应 docs/architecture/kernel_interface.md 的验收标准 §8.4 第 1 条：
`e_type=ET_EXEC`、`e_machine=x86-64`、`e_entry` 落在某个可执行 `PT_LOAD` 段内。

为什么不依赖工具链：`readelf`/`file` 的输出措辞随 binutils 版本变化，不适合作为
CI 断言；这里直接解析结构。

用法:
    python3 tests/check_kernel_elf.py <kernel-elf> [more...]
"""

import struct
import sys

ET_EXEC = 2
EM_X86_64 = 62
ELFCLASS64 = 2
ELFDATA2LSB = 1
PT_LOAD = 1
PF_X = 1


class CheckError(Exception):
    """校验失败。"""


def check(path):
    with open(path, "rb") as handle:
        data = handle.read()

    if len(data) < 64:
        raise CheckError("文件过小，连 ELF 头都不完整")
    if data[:4] != b"\x7fELF":
        raise CheckError("缺少 ELF magic")
    if data[4] != ELFCLASS64:
        raise CheckError(f"不是 64 位 ELF（EI_CLASS={data[4]}）")
    if data[5] != ELFDATA2LSB:
        raise CheckError(f"不是小端 ELF（EI_DATA={data[5]}）")

    e_type, e_machine = struct.unpack_from("<HH", data, 16)
    e_entry = struct.unpack_from("<Q", data, 24)[0]
    e_phoff = struct.unpack_from("<Q", data, 32)[0]
    e_phentsize, e_phnum = struct.unpack_from("<HH", data, 54)

    if e_type != ET_EXEC:
        raise CheckError(f"e_type 应为 ET_EXEC({ET_EXEC})，实际 {e_type}（PIE/ET_DYN 需重定位，不支持）")
    if e_machine != EM_X86_64:
        raise CheckError(f"e_machine 应为 x86-64({EM_X86_64})，实际 {e_machine}")
    if e_phentsize < 56 or e_phnum == 0:
        raise CheckError("Program Header 表缺失或步长过小")

    load_segments = 0
    entry_ok = False
    for index in range(e_phnum):
        offset = e_phoff + index * e_phentsize
        if offset + 56 > len(data):
            raise CheckError("Program Header 表越界")
        p_type, p_flags = struct.unpack_from("<II", data, offset)
        p_offset, p_vaddr, _p_paddr, p_filesz, p_memsz = struct.unpack_from("<QQQQQ", data, offset + 8)
        if p_type != PT_LOAD:
            continue
        load_segments += 1
        if p_offset + p_filesz > len(data):
            raise CheckError(f"段 {index} 的数据超出文件长度")
        if p_filesz > p_memsz:
            raise CheckError(f"段 {index} 的 p_filesz > p_memsz")
        if p_flags & PF_X and p_vaddr <= e_entry < p_vaddr + p_memsz:
            entry_ok = True

    if load_segments == 0:
        raise CheckError("没有任何 PT_LOAD 段")
    if not entry_ok:
        raise CheckError(f"入口 0x{e_entry:X} 不在任何可执行 PT_LOAD 段内")

    print(
        f"  ✅ {path}: ELF64/x86-64/ET_EXEC / 入口 0x{e_entry:X} 在可执行段内 / "
        f"{load_segments} 个 PT_LOAD 段 / {len(data)} 字节"
    )


def main(argv):
    if len(argv) < 2:
        print(__doc__.strip(), file=sys.stderr)
        return 2

    failed = 0
    for path in argv[1:]:
        try:
            check(path)
        except (CheckError, OSError, struct.error) as err:
            print(f"  ❌ {path}: {err}", file=sys.stderr)
            failed += 1
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
