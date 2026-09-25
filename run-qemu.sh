#!/usr/bin/env bash
#
# ParanukOS 启动脚本：把编译产物包装成符合 UEFI 规范的 ESP 虚拟盘并交给 QEMU。
#
# 用法:
#   cargo run                  # 由 .cargo/config.toml 的 runner 自动调用
#   ./run-qemu.sh <path.efi>   # 手动指定产物
#
# 退出模拟器：先按 Ctrl + A，再按 X。
# 切勿使用 Ctrl + C —— 会造成僵尸 QEMU 进程在后台死锁并霸占串口。
#
# 依赖（按发行版）:
#   Ubuntu/Debian: sudo apt install qemu-system-x86 ovmf
#   Fedora/RHEL:   sudo dnf install qemu-system-x86 edk2-ovmf

set -euo pipefail

EFI_PATH="${1:-}"
if [ -z "${EFI_PATH}" ]; then
    echo "[-] 用法: $0 <path-to-efi-binary>" >&2
    exit 2
fi
if [ ! -f "${EFI_PATH}" ]; then
    echo "[-] 找不到 EFI 产物: ${EFI_PATH}" >&2
    exit 2
fi

# --- 依赖检查：给出可直接执行的修复建议，而不是让人猜 ---
if ! command -v qemu-system-x86_64 >/dev/null 2>&1; then
    echo "[-] 未找到 qemu-system-x86_64。" >&2
    echo "    Ubuntu/Debian: sudo apt install qemu-system-x86 ovmf" >&2
    echo "    Fedora/RHEL:   sudo dnf install qemu-system-x86 edk2-ovmf" >&2
    exit 1
fi

# OVMF 固件路径在各发行版/各版本间并不统一，按序探测；可用 OVMF_CODE 覆盖。
OVMF="${OVMF_CODE:-}"
if [ -z "${OVMF}" ]; then
    for candidate in \
        /usr/share/OVMF/OVMF_CODE.fd \
        /usr/share/OVMF/OVMF_CODE_4M.fd \
        /usr/share/OVMF/OVMF_CODE.secboot.fd \
        /usr/share/edk2/ovmf/OVMF_CODE.fd \
        /usr/share/edk2/x64/OVMF_CODE.fd \
        /usr/share/qemu/OVMF.fd
    do
        if [ -f "${candidate}" ]; then
            OVMF="${candidate}"
            break
        fi
    done
fi

if [ -z "${OVMF}" ] || [ ! -f "${OVMF}" ]; then
    echo "[-] 未找到 OVMF 固件（已尝试各发行版常见路径）。" >&2
    echo "    Ubuntu/Debian: sudo apt install ovmf" >&2
    echo "    Fedora/RHEL:   sudo dnf install edk2-ovmf" >&2
    echo "    也可以用环境变量指定: OVMF_CODE=/path/to/OVMF_CODE.fd $0 <efi>" >&2
    exit 1
fi

# 建立符合 UEFI 规范的标准 ESP 目录树。
# UEFI 规定可移动介质默认启动路径为: /EFI/BOOT/BOOTX64.EFI
ESP_DIR="${TMPDIR:-/tmp}/paranukos_esp"
rm -rf "${ESP_DIR}"
mkdir -p "${ESP_DIR}/EFI/BOOT"
cp "${EFI_PATH}" "${ESP_DIR}/EFI/BOOT/BOOTX64.EFI"

echo "[+] ParanukOS: ESP 虚拟盘已就绪 (${ESP_DIR})"
echo "[+] OVMF 固件: ${OVMF}"
echo "[+] 启动 QEMU ... 退出请按 Ctrl + A 再按 X"

set +e
qemu-system-x86_64 \
    -bios "${OVMF}" \
    -net none \
    -nographic \
    -drive format=raw,file=fat:rw:"${ESP_DIR}"
status=$?
set -e

# 冒烟测试目前依赖 timeout 终止 QEMU（应用加载完成后自旋等待），
# 因此 0（手动退出）与 124/137（被 timeout 终止）都视为启动成功。
# TODO: 接入 isa-debug-exit 后按退出码精确判定，不再吞掉真实崩溃。
case "${status}" in
    0 | 124 | 137) exit 0 ;;
    *) exit "${status}" ;;
esac
