#!/usr/bin/env bash
#
# ParanukOS QEMU 冒烟测试（Milestone 0）。
#
# 引导器在 `--features qemu-exit` 下通过 QEMU 的 isa-debug-exit 设备报告结果，
# 内核也通过 `BootInfo.exit_port` 用同一机制报告自检结果，因此这里断言**精确退出码**：
#
#   33  引导器装载完成（M0 起正常路径不再使用：成功会直接跳入内核）
#   35  内核镜像装载失败（缺少镜像 / 非 ELF / 段布局非法 / 入口不可执行）
#   37  内核自检通过（BootInfo 有效 + 内存图可用 + RSDP 存在）
#   39  内核自检失败（magic/version 不匹配等）
#   124 超时：应用没有主动退出（判失败）
#
# 退出码常量与 crates/boot-info 保持一致。
#
# 用法: bash tests/smoke.sh
# 环境变量:
#   BOOT_TIMEOUT  单个用例超时秒数（默认 60）
#   LOG_DIR       串口日志目录（默认 target/smoke-logs，CI 失败时上传）

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

UEFI_TARGET="x86_64-unknown-uefi"
BARE_TARGET="x86_64-unknown-none"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-60}"
LOG_DIR="${LOG_DIR:-${ROOT}/target/smoke-logs}"
mkdir -p "$LOG_DIR"

EXIT_LOAD_FAILURE=35
EXIT_KERNEL_OK=37
EXIT_KERNEL_FAILURE=39

WORK="$(mktemp -d)"
ESP_DIR="$(mktemp -d)/esp"

cleanup() {
    rm -rf "$WORK"
    rm -rf "$(dirname "$ESP_DIR")"
}
trap cleanup EXIT

pass=0
fail=0
ok() {
    printf '  ✅ %s\n' "$1"
    pass=$((pass + 1))
}
bad() {
    printf '  ❌ %s\n' "$1"
    fail=$((fail + 1))
}
show_log() {
    echo "    --- $1 串口输出（尾部） ---"
    sed 's/\x1b\[[0-9;]*[A-Za-z]//g' "$1" | tail -20 | sed 's/^/    /'
}

# --- 依赖检查 ---
if ! command -v qemu-system-x86_64 >/dev/null 2>&1; then
    echo "[-] 未安装 qemu-system-x86_64，无法运行冒烟测试。" >&2
    echo "    Ubuntu/Debian: sudo apt install qemu-system-x86 ovmf" >&2
    echo "    Fedora/RHEL:   sudo dnf install qemu-system-x86 edk2-ovmf" >&2
    exit 1
fi
if ! command -v rustc >/dev/null 2>&1 || ! command -v cargo >/dev/null 2>&1; then
    echo "[-] 未找到 rustc/cargo。" >&2
    exit 1
fi

echo "==> 1/5 构建内核（$BARE_TARGET）"
if ! cargo build -p kernel --target "$BARE_TARGET"; then
    echo "[-] 内核构建失败。" >&2
    exit 1
fi
cp "target/${BARE_TARGET}/debug/kernel" "$WORK/KERNEL.ELF"

echo "==> 1.5/5 静态校验内核 ELF（验收标准 §8.4 第 1 条）"
if ! python3 tests/check_kernel_elf.py "$WORK/KERNEL.ELF"; then
    echo "[-] 内核 ELF 未通过静态校验。" >&2
    exit 1
fi

echo "==> 2/5 构建引导器（$UEFI_TARGET，启用 qemu-exit）"
if ! cargo build --target "$UEFI_TARGET" --features qemu-exit; then
    echo "[-] 引导器构建失败。" >&2
    exit 1
fi
cp "target/${UEFI_TARGET}/debug/paranukos.efi" "$WORK/loader.efi"

echo "==> 3/5 构建「注入错误 magic」的引导器（验证内核自检失败路径）"
if ! cargo build --target "$UEFI_TARGET" --features qemu-exit,inject-bad-magic; then
    echo "[-] 注入版引导器构建失败。" >&2
    exit 1
fi
cp "target/${UEFI_TARGET}/debug/paranukos.efi" "$WORK/loader-bad-magic.efi"

echo "==> 4/5 准备用例数据"
# 非 ELF 内容，且必须 >= 64 字节：否则会先命中「镜像过小」而不是 magic 校验
head -c 128 /dev/zero >"$WORK/not-elf.bin"
echo "    内核: $WORK/KERNEL.ELF ($(wc -c <"$WORK/KERNEL.ELF") 字节)"

# 启动一次并断言精确退出码。
# 用法: boot_case <efi> <kernel_elf|-> <日志> <期望退出码> <描述>
boot_case() {
    local efi="$1" kernel="$2" log="$3" want="$4" desc="$5"
    if [ "$kernel" = "-" ]; then
        export KERNEL_ELF=""   # 显式禁用自动投放 → ESP 中没有 KERNEL.ELF
    else
        export KERNEL_ELF="$kernel"
    fi
    : >"$log"
    timeout "$BOOT_TIMEOUT" env ESP_DIR="$ESP_DIR" ./run-qemu.sh "$efi" >"$log" 2>&1
    local got=$?
    if [ "$got" -eq "$want" ]; then
        ok "$desc（退出码 $got）"
        return 0
    fi
    if [ "$got" -eq 124 ]; then
        bad "$desc：超时 ${BOOT_TIMEOUT}s 仍未退出"
    else
        bad "$desc：退出码 $got，期望 $want"
    fi
    show_log "$log"
    return 1
}

grep_log() { # <日志> <正则> <描述>
    if grep -qE "$2" "$1"; then
        ok "$3"
    else
        bad "$3"
        show_log "$1"
    fi
}

echo "==> 5/5 引导用例"

# --- A. 正向：合法内核 → 内核自检通过（37） ---
boot_case "$WORK/loader.efi" "$WORK/KERNEL.ELF" "$LOG_DIR/m0-positive.log" "$EXIT_KERNEL_OK" \
    "合法内核：引导器装载并跳转，内核自检通过"
grep_log "$LOG_DIR/m0-positive.log" '个 PT_LOAD 段' "引导器按段解析 ELF"
grep_log "$LOG_DIR/m0-positive.log" '交接准备就绪' "引导器完成交接准备"
grep_log "$LOG_DIR/m0-positive.log" '\[kernel\] ParanukOS kernel alive' "内核真的开始执行"
grep_log "$LOG_DIR/m0-positive.log" '\[kernel\] self-check OK' "内核自检通过"
# 内存图条目数必须 > 0（形如 "memory map: 127 项"）
if grep -qE 'memory map: [1-9][0-9]* 项' "$LOG_DIR/m0-positive.log"; then
    ok "内核读到非空内存图"
else
    bad "内存图条目数不是 > 0"
    show_log "$LOG_DIR/m0-positive.log"
fi
# RSDP 必须非零
if grep -qE 'rsdp=0x0*[1-9a-fA-F]' "$LOG_DIR/m0-positive.log"; then
    ok "内核读到非零 ACPI RSDP"
else
    bad "RSDP 为零"
    show_log "$LOG_DIR/m0-positive.log"
fi

# --- B. 反向 1：ESP 中没有内核镜像 → 装载失败（35） ---
boot_case "$WORK/loader.efi" "-" "$LOG_DIR/m0-no-kernel.log" "$EXIT_LOAD_FAILURE" \
    "缺少内核镜像：以 35 失败"
grep_log "$LOG_DIR/m0-no-kernel.log" '内核装载失败' "报告了失败原因"

# --- C. 反向 2：内核镜像存在但不是合法 ELF → 装载失败（35） ---
boot_case "$WORK/loader.efi" "$WORK/not-elf.bin" "$LOG_DIR/m0-bad-elf.log" "$EXIT_LOAD_FAILURE" \
    "非 ELF 镜像：以 35 失败"
grep_log "$LOG_DIR/m0-bad-elf.log" '缺少 ELF magic' "报错指明了具体原因（缺少 ELF magic）"
if grep -q '\[kernel\]' "$LOG_DIR/m0-bad-elf.log"; then
    bad "非法镜像却仍跳入了内核"
else
    ok "非法镜像未跳入内核"
fi

# --- D. 反向 3：BootInfo.magic 被注入错误 → 内核自检失败（39） ---
boot_case "$WORK/loader-bad-magic.efi" "$WORK/KERNEL.ELF" "$LOG_DIR/m0-bad-magic.log" "$EXIT_KERNEL_FAILURE" \
    "错误的 BootInfo.magic：内核自检失败并以 39 退出"
grep_log "$LOG_DIR/m0-bad-magic.log" '\[kernel\] self-check FAILED' "内核报告了自检失败"
grep_log "$LOG_DIR/m0-bad-magic.log" 'magic 不匹配' "失败原因指明是 magic 不匹配"

echo
echo "结果: ${pass} 项通过, ${fail} 项失败"
if [ "$fail" -ne 0 ]; then
    exit 1
fi
exit 0
