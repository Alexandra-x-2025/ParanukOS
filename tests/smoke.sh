#!/usr/bin/env bash
#
# ParanukOS QEMU 冒烟测试（Milestone 0）。
#
# 引导器在 `--features qemu-exit` 下通过 QEMU 的 isa-debug-exit 设备报告结果，
# 内核也通过 `BootInfo.exit_port` 用同一机制报告自检结果，因此这里断言**精确退出码**：
#
#   33  引导器装载完成（M0 起正常路径不再使用：成功会直接跳入内核）
#   35  内核镜像装载失败（缺少镜像 / 非 ELF / 段布局非法 / 入口不可执行）
#   37  内核自检通过（BootInfo 有效 + 内存图可用 + RSDP 存在 + 自建页表已安装）
#   39  内核自检失败（magic/version 不匹配等）
#   41  内核发生未处理异常或 panic（意外崩溃）
#   43  内核内存初始化失败（页表构建/安装、页帧分配器、内核堆）
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
EXIT_KERNEL_FAULT=41
EXIT_KERNEL_MEMORY_FAILURE=43

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
grep_log "$LOG_DIR/m0-positive.log" 'IDT 已安装' "内核安装了 IDT（M1）"
grep_log "$LOG_DIR/m0-positive.log" 'self-check OK' "内核自检通过"
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

# --- M2a：内核自建的恒等映射页表（memory_subsystem.md §8.3） ---
grep_log "$LOG_DIR/m0-positive.log" 'paging: 恒等映射' "内核建立并安装了恒等映射页表"
if grep -qE 'paging: 上限 0x[0-9A-F]+，页表 7 页，CR3=0x[0-9A-F]+' "$LOG_DIR/m0-positive.log"; then
    ok "页表占用符合预算（7 页 = 28 KiB）"
else
    bad "页表页数不是预算内的 7 页"
    show_log "$LOG_DIR/m0-positive.log"
fi
grep_log "$LOG_DIR/m0-positive.log" '切换 CR3 后 BootInfo 仍可读' \
    "恒等映射确实覆盖了交接结构（切换 CR3 后可重新校验 BootInfo）"
# 映射规模必须覆盖测试虚拟机的主要内存（QEMU 默认 128 MiB，这里放宽到 64 MiB）
mapped_mib="$(sed -n 's/.*个 2 MiB 大块（\([0-9]*\) MiB.*/\1/p' "$LOG_DIR/m0-positive.log" | head -1)"
if [ -n "$mapped_mib" ] && [ "$mapped_mib" -ge 64 ]; then
    ok "恒等映射规模 ${mapped_mib} MiB（≥ 64 MiB）"
else
    bad "恒等映射规模不足或无法解析（读到 '${mapped_mib:-空}'）"
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
grep_log "$LOG_DIR/m0-bad-magic.log" 'self-check FAILED' "内核报告了自检失败"
grep_log "$LOG_DIR/m0-bad-magic.log" 'magic 不匹配' "失败原因指明是 magic 不匹配"

# --- E. 故障注入：内核执行 ud2 → 异常处理器报告并以 41 退出 ---
echo "==> 附加用例：异常处理器（故障注入）"
if ! cargo build -p kernel --target "$BARE_TARGET" --features inject-fault; then
    echo "[-] 注入故障的内核构建失败。" >&2
    exit 1
fi
cp "target/${BARE_TARGET}/debug/kernel" "$WORK/KERNEL-fault.ELF"
boot_case "$WORK/loader.efi" "$WORK/KERNEL-fault.ELF" "$LOG_DIR/m1-fault.log" "$EXIT_KERNEL_FAULT" \
    "内核触发 #UD：异常处理器报告并以 41 退出"
grep_log "$LOG_DIR/m1-fault.log" '未处理的 CPU 异常' "打印了异常诊断"
grep_log "$LOG_DIR/m1-fault.log" '#UD' "指认了向量（#UD 非法指令）"
grep_log "$LOG_DIR/m1-fault.log" 'rip=0x' "打印了出错指令地址"

# --- F. 故障注入：内核页表生效后解引用空指针 → #PF → 41（M2a） ---
#
# 页 0 按策略永不映射（memory_subsystem.md §3.4）。固件的恒等映射通常会把页 0 也映射，
# 所以"读地址 0 会 #PF"正是"生效的是内核自己的页表"的直接证据。
echo "==> 附加用例：内核自建页表（故障注入）"
if ! cargo build -p kernel --target "$BARE_TARGET" --features inject-null-deref; then
    echo "[-] 注入空指针访问的内核构建失败。" >&2
    exit 1
fi
cp "target/${BARE_TARGET}/debug/kernel" "$WORK/KERNEL-null.ELF"
boot_case "$WORK/loader.efi" "$WORK/KERNEL-null.ELF" "$LOG_DIR/m2a-null-deref.log" "$EXIT_KERNEL_FAULT" \
    "读取未映射的页 0：内核自己的页表生效并以 41 退出"
grep_log "$LOG_DIR/m2a-null-deref.log" 'paging: 恒等映射' "崩溃前已完成页表安装"
grep_log "$LOG_DIR/m2a-null-deref.log" '#PF 页错误' "指认了向量（#PF 页错误）"
grep_log "$LOG_DIR/m2a-null-deref.log" 'cr2=0x0 ' "CR2 指出出错地址正是页 0"

# 只在注入版里出现的提示，用来确认我们确实走到了那条路径，而不是别的原因导致的 #PF
grep_log "$LOG_DIR/m2a-null-deref.log" '\[inject\] 故意读取未映射的页 0' "命中注入路径"

echo
echo "结果: ${pass} 项通过, ${fail} 项失败"
if [ "$fail" -ne 0 ]; then
    exit 1
fi
exit 0
