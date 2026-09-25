#!/usr/bin/env bash
#
# ParanukOS QEMU 冒烟测试。
#
# 引导器在 `--features qemu-exit` 下通过 QEMU 的 isa-debug-exit 设备报告结果，
# 因此这里断言**精确退出码**，而不是猜日志文本：
#
#   33   引导成功（读盘 + ELF 校验 + 载入内存全部完成）
#   35   内核加载失败（缺少镜像，或镜像不是合法的 ELF64/x86-64/ET_EXEC）
#   124  超时：应用没有主动退出（判失败）
#
# 同时仍然检查串口输出中的关键信息，确保失败原因是可读的。
#
# 用法: bash tests/smoke.sh
# 环境变量:
#   BOOT_TIMEOUT  单个用例的超时秒数（默认 60）
#   LOG_DIR       串口日志目录（默认 target/smoke-logs，CI 失败时上传）

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET="x86_64-unknown-uefi"
EFI="target/${TARGET}/debug/paranukos.efi"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-60}"
LOG_DIR="${LOG_DIR:-${ROOT}/target/smoke-logs}"
mkdir -p "$LOG_DIR"

# 与 src/main.rs 中的 QEMU_EXIT_* 常量保持一致：QEMU 退出码 = (value << 1) | 1
EXIT_SUCCESS=33       # 0x10
EXIT_LOAD_FAILURE=35  # 0x11

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
    tail -20 "$1" | sed 's/^/    /'
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

echo "==> 1/4 构建 EFI 镜像（启用 qemu-exit 特性）"
if ! cargo build --target "$TARGET" --features qemu-exit; then
    echo "[-] 构建失败。" >&2
    exit 1
fi
if [ ! -f "$EFI" ]; then
    echo "[-] 未找到构建产物: $EFI" >&2
    exit 1
fi
echo "    产物: $EFI"

echo "==> 2/4 准备测试用内核镜像"
# x86_64-unknown-none 默认产出 PIE(ET_DYN)，而引导器要求 ET_EXEC，
# 因此必须加 -C relocation-model=static。
if ! rustc \
    --target x86_64-unknown-none \
    -O \
    -C panic=abort \
    -C relocation-model=static \
    --edition 2021 \
    tests/fixtures/dummy_kernel.rs \
    -o "$WORK/KERNEL.ELF"; then
    echo "[-] 测试用内核镜像编译失败。" >&2
    exit 1
fi
mkdir -p "$WORK/esp-good/EFI/PARANUKO"
cp "$WORK/KERNEL.ELF" "$WORK/esp-good/EFI/PARANUKO/KERNEL.ELF"

# 非 ELF 内容，且必须 >= 64 字节：否则会先命中「镜像过小」而不是 magic 校验
mkdir -p "$WORK/esp-bad/EFI/PARANUKO"
head -c 128 /dev/zero >"$WORK/esp-bad/EFI/PARANUKO/KERNEL.ELF"

# 启动一次并断言精确退出码。
# 用法: boot_case <esp_extra|-> <日志文件> <期望退出码> <描述>
boot_case() {
    local extra="$1" log="$2" want="$3" desc="$4"
    if [ "$extra" = "-" ]; then
        unset ESP_EXTRA
    else
        export ESP_EXTRA="$extra"
    fi
    : >"$log"
    timeout "$BOOT_TIMEOUT" env ESP_DIR="$ESP_DIR" ./run-qemu.sh "$EFI" >"$log" 2>&1
    local got=$?
    if [ "$got" -eq "$want" ]; then
        ok "$desc（退出码 $got）"
        return 0
    fi
    if [ "$got" -eq 124 ]; then
        bad "$desc：超时 ${BOOT_TIMEOUT}s 仍未退出（应用卡住了？）"
    else
        bad "$desc：退出码 $got，期望 $want"
    fi
    show_log "$log"
    return 1
}

echo "==> 3/4 正向用例：ESP 中存在合法内核镜像"
boot_case "$WORK/esp-good" "$LOG_DIR/positive.log" "$EXIT_SUCCESS" "引导成功并按约定退出码结束"
if grep -q 'ELF64/x86-64 校验通过' "$LOG_DIR/positive.log"; then
    ok "ELF64/x86-64 头部校验通过"
else
    bad "缺少 ELF 头部校验日志"
fi
if grep -q '内核已载入 0x' "$LOG_DIR/positive.log"; then
    ok "镜像已复制到分配的物理页"
else
    bad "缺少镜像载入日志"
fi

echo "==> 4/4 反向用例"
boot_case "-" "$LOG_DIR/no-kernel.log" "$EXIT_LOAD_FAILURE" "缺少内核镜像：失败退出"
if grep -q '内核加载失败' "$LOG_DIR/no-kernel.log"; then
    ok "报告了失败"
else
    bad "缺少失败日志"
fi

boot_case "$WORK/esp-bad" "$LOG_DIR/bad-image.log" "$EXIT_LOAD_FAILURE" "非 ELF 镜像：失败退出"
if grep -q '内核加载失败' "$LOG_DIR/bad-image.log"; then
    ok "报告了失败"
else
    bad "缺少失败日志"
fi
# 报错必须是可读原因（main 上的 LoadError 已实现 Display）
if grep -q '缺少 ELF magic' "$LOG_DIR/bad-image.log"; then
    ok "报错指明了具体原因（缺少 ELF magic）"
else
    bad "报错未指明具体原因"
fi

if grep -q '内核镜像校验通过' "$LOG_DIR/no-kernel.log" "$LOG_DIR/bad-image.log"; then
    bad "反向用例却报告成功"
else
    ok "反向用例未误报成功"
fi

echo
echo "结果: ${pass} 项通过, ${fail} 项失败"
if [ "$fail" -ne 0 ]; then
    exit 1
fi
exit 0
