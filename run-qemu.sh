#!/usr/bin/env bash
#
# ParanukOS 启动脚本：把编译产物包装成符合 UEFI 规范的 ESP 虚拟盘并交给 QEMU。
#
# 用法:
#   cargo run                  # 由 .cargo/config.toml 的 runner 自动调用
#   ./run-qemu.sh <path.efi>   # 手动指定产物
#
# 环境变量:
#   OVMF_CODE     指定 OVMF CODE 固件路径（覆盖自动探测）
#   OVMF_VARS     指定 OVMF VARS 变量存储模板（默认与 CODE 同目录同名替换）
#   ESP_DIR       虚拟盘目录（默认 ${TMPDIR:-/tmp}/paranukos_esp）
#   ESP_EXTRA     目录，其内容会被一并复制进 ESP 根目录；
#                 冒烟测试用它放入 \EFI\PARANUKO\KERNEL.ELF
#   QEMU_DATA_DIR QEMU 数据目录（仅在非标准前缀安装时需要，传给 -L）
#
# 注意：OVMF 的 4M 固件必须通过 pflash 驱动加载，不能用 -bios
# （-bios 会把文件当传统 BIOS 镜像处理，直接报 "could not load PC BIOS"）。
# 若 QEMU 是从非标准前缀运行的，还需自行设置 QEMU_MODULE_DIR。
#
# 退出模拟器：先按 Ctrl + A，再按 X（退出码 0）。
# 退出码含义见文件末尾。
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
OVMF_CODE="${OVMF_CODE:-}"
if [ -z "${OVMF_CODE}" ]; then
    for candidate in \
        /usr/share/OVMF/OVMF_CODE_4M.fd \
        /usr/share/OVMF/OVMF_CODE.fd \
        /usr/share/OVMF/OVMF_CODE.secboot.fd \
        /usr/share/edk2/ovmf/OVMF_CODE_4M.fd \
        /usr/share/edk2/ovmf/OVMF_CODE.fd \
        /usr/share/edk2/x64/OVMF_CODE.fd \
        /usr/share/qemu/OVMF.fd
    do
        if [ -f "${candidate}" ]; then
            OVMF_CODE="${candidate}"
            break
        fi
    done
fi

if [ -z "${OVMF_CODE}" ] || [ ! -f "${OVMF_CODE}" ]; then
    echo "[-] 未找到 OVMF 固件（已尝试各发行版常见路径）。" >&2
    echo "    Ubuntu/Debian: sudo apt install ovmf" >&2
    echo "    Fedora/RHEL:   sudo dnf install edk2-ovmf" >&2
    echo "    也可以用环境变量指定: OVMF_CODE=/path/to/OVMF_CODE.fd $0 <efi>" >&2
    exit 1
fi

# 建立符合 UEFI 规范的标准 ESP 目录树。
# UEFI 规定可移动介质默认启动路径为: /EFI/BOOT/BOOTX64.EFI
ESP_DIR="${ESP_DIR:-${TMPDIR:-/tmp}/paranukos_esp}"
rm -rf "${ESP_DIR}"
mkdir -p "${ESP_DIR}/EFI/BOOT"
cp "${EFI_PATH}" "${ESP_DIR}/EFI/BOOT/BOOTX64.EFI"

# 可选：把 ESP_EXTRA 目录内容合并进来（冒烟测试用于放置内核镜像）。
if [ -n "${ESP_EXTRA:-}" ]; then
    if [ ! -d "${ESP_EXTRA}" ]; then
        echo "[-] ESP_EXTRA 不是目录: ${ESP_EXTRA}" >&2
        exit 2
    fi
    cp -a "${ESP_EXTRA}/." "${ESP_DIR}/"
fi

# 可写的变量存储：pflash 需要一份可写 VARS，且每次运行都从模板重新拷贝，
# 避免上一次运行留下的 NVRAM 状态影响下一次（对可重复的冒烟测试很重要）。
WORK_DIR="$(mktemp -d)"
VARS_COPY="${WORK_DIR}/OVMF_VARS.fd"
VARS_SOURCE="${OVMF_VARS:-}"
if [ -z "${VARS_SOURCE}" ]; then
    derived="${OVMF_CODE/_CODE/_VARS}"
    if [ "${derived}" != "${OVMF_CODE}" ] && [ -f "${derived}" ]; then
        VARS_SOURCE="${derived}"
    fi
fi
if [ -n "${VARS_SOURCE}" ] && [ -f "${VARS_SOURCE}" ]; then
    cp "${VARS_SOURCE}" "${VARS_COPY}"
else
    # 没有配套 VARS 时，用与 CODE 等大的空白文件充当变量存储（OVMF 会自行初始化）
    truncate -s "$(wc -c <"${OVMF_CODE}")" "${VARS_COPY}"
fi

echo "[+] ParanukOS: ESP 虚拟盘已就绪 (${ESP_DIR})"
echo "[+] OVMF 固件: ${OVMF_CODE}"
echo "[+] 启动 QEMU ... 退出请按 Ctrl + A 再按 X"

qemu_args=()
if [ -n "${QEMU_DATA_DIR:-}" ]; then
    qemu_args+=(-L "${QEMU_DATA_DIR}")
fi

# QEMU 作为子进程运行：脚本被 timeout/kill（含 CI 用例）终止时能一并清理，
# 避免留下僵尸 QEMU 进程占用串口。
# isa-debug-exit 的端口必须与 src/main.rs / uefi 的 qemu 特性一致（都是 0xF4）：
# QEMU 8.2 起该设备默认的 iobase 是 0x501，不显式指定的话应用写入 0xF4 不会生效。
qemu-system-x86_64 \
    "${qemu_args[@]}" \
    -machine q35 \
    -vga none \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
    -drive if=pflash,format=raw,readonly=on,file="${OVMF_CODE}" \
    -drive if=pflash,format=raw,file="${VARS_COPY}" \
    -net none \
    -nographic \
    -drive format=raw,file=fat:rw:"${ESP_DIR}" &
qemu_pid=$!

cleanup() {
    if kill -0 "${qemu_pid}" 2>/dev/null; then
        kill "${qemu_pid}" 2>/dev/null || true
        wait "${qemu_pid}" 2>/dev/null || true
    fi
    rm -rf "${WORK_DIR}"
}
trap cleanup EXIT INT TERM

set +e
wait "${qemu_pid}"
status=$?
set -e

# 原样上报 QEMU 的退出码。配合 `-device isa-debug-exit`，退出码有明确含义：
#   0    人类退出（Ctrl + A 然后 X），或固件正常结束
#   33   引导器报告成功（读取 + ELF 校验 + 载入内存全部完成）
#   35   引导器报告内核加载失败
#   9    panic（uefi 的 qemu 特性以 0x04 作为失败码，(4 << 1) | 1 = 9）
#   124  被 timeout 终止 —— 应用没有主动退出，通常意味着卡死
# 注意：不再把 124/137 归一化成成功，否则会掩盖真实故障。
exit "${status}"
