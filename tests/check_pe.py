#!/usr/bin/env python3
"""校验 UEFI 镜像是否为固件可加载的 PE32+ EFI 应用。

``file`` 命令的输出措辞会随 binutils 版本变化，不适合作为 CI 断言；
这里直接解析 PE 结构，逐项校验，失败时给出明确原因。

用法:
    python3 tests/check_pe.py <image.efi> [more.efi ...]
"""

import struct
import sys

PE32_PLUS_MAGIC = 0x20B
MACHINE_X86_64 = 0x8664
SUBSYSTEM_EFI_APPLICATION = 10

IMAGE_SCN_CNT_CODE = 0x00000020
IMAGE_SCN_MEM_EXECUTE = 0x20000000

IMPORT_DIRECTORY_INDEX = 1


class CheckError(Exception):
    """校验失败。"""


def read_section_headers(data, coff_off, optional_size, section_count):
    sections = []
    off = coff_off + 20 + optional_size
    for _ in range(section_count):
        if off + 40 > len(data):
            raise CheckError("节表超出文件长度")
        name = data[off : off + 8].rstrip(b"\0").decode("ascii", "replace")
        virtual_size = struct.unpack_from("<I", data, off + 8)[0]
        virtual_address = struct.unpack_from("<I", data, off + 12)[0]
        raw_size = struct.unpack_from("<I", data, off + 16)[0]
        characteristics = struct.unpack_from("<I", data, off + 36)[0]
        sections.append(
            {
                "name": name,
                "virtual_size": virtual_size,
                "virtual_address": virtual_address,
                "raw_size": raw_size,
                "characteristics": characteristics,
            }
        )
        off += 40
    return sections


def check(path):
    with open(path, "rb") as handle:
        data = handle.read()

    if len(data) < 0x40:
        raise CheckError("文件过小，连 DOS 头都不完整")
    if data[:2] != b"MZ":
        raise CheckError("缺少 DOS magic 'MZ'")

    pe_off = struct.unpack_from("<I", data, 0x3C)[0]
    if pe_off + 24 > len(data):
        raise CheckError("e_lfanew 越界")
    if data[pe_off : pe_off + 4] != b"PE\0\0":
        raise CheckError("缺少 PE 签名")

    coff_off = pe_off + 4
    machine = struct.unpack_from("<H", data, coff_off)[0]
    section_count = struct.unpack_from("<H", data, coff_off + 2)[0]
    optional_size = struct.unpack_from("<H", data, coff_off + 16)[0]

    optional_off = coff_off + 20
    if optional_off + optional_size > len(data):
        raise CheckError("可选头越界")

    if machine != MACHINE_X86_64:
        raise CheckError(f"机器类型应为 0x{MACHINE_X86_64:X}（x86-64），实际 0x{machine:X}")

    magic = struct.unpack_from("<H", data, optional_off)[0]
    if magic != PE32_PLUS_MAGIC:
        raise CheckError(f"可选头魔数应为 0x{PE32_PLUS_MAGIC:X}（PE32+），实际 0x{magic:X}")

    entry_point = struct.unpack_from("<I", data, optional_off + 0x10)[0]
    if entry_point == 0:
        raise CheckError("入口点 RVA 为 0")

    section_alignment = struct.unpack_from("<I", data, optional_off + 0x20)[0]
    file_alignment = struct.unpack_from("<I", data, optional_off + 0x24)[0]
    subsystem = struct.unpack_from("<H", data, optional_off + 0x44)[0]

    if subsystem != SUBSYSTEM_EFI_APPLICATION:
        raise CheckError(
            f"子系统应为 {SUBSYSTEM_EFI_APPLICATION}（EFI application），实际 {subsystem}"
        )
    if section_alignment == 0 or section_alignment & (section_alignment - 1) != 0:
        raise CheckError(f"节对齐必须是 2 的幂，实际 0x{section_alignment:X}")
    if file_alignment == 0 or file_alignment & (file_alignment - 1) != 0:
        raise CheckError(f"文件对齐必须是 2 的幂，实际 0x{file_alignment:X}")

    sections = read_section_headers(data, coff_off, optional_size, section_count)

    entry_section = None
    for section in sections:
        start = section["virtual_address"]
        end = start + max(section["virtual_size"], section["raw_size"])
        if start <= entry_point < end:
            entry_section = section
            break
    if entry_section is None:
        raise CheckError(f"入口点 RVA 0x{entry_point:X} 不落在任何节内")
    if not entry_section["characteristics"] & IMAGE_SCN_MEM_EXECUTE:
        raise CheckError(f"入口点所在节 {entry_section['name']} 未标记为可执行")

    number_of_directories = struct.unpack_from("<I", data, optional_off + 0x6C)[0]
    if number_of_directories > IMPORT_DIRECTORY_INDEX:
        import_rva, import_size = struct.unpack_from(
            "<II", data, optional_off + 0x70 + IMPORT_DIRECTORY_INDEX * 8
        )
        if import_rva != 0 or import_size != 0:
            raise CheckError("UEFI 镜像不应包含导入表")

    print(
        f"  ✅ {path}: PE32+ / x86-64 / subsystem={subsystem}(EFI app) / "
        f"入口 0x{entry_point:X} 位于 {entry_section['name']} / "
        f"节数 {section_count} / {len(data)} 字节 / 无导入表"
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
