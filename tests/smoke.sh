#!/usr/bin/env bash
#
# ParanukOS QEMU 冒烟测试。
#
# 断言两件事：
#   正向：ESP 中放入合法 ELF64 内核镜像时，引导器能启动、读盘、校验 ELF 并报告成功；
#   反向：缺少内核镜像时，引导器能优雅报错，而不是挂死、崩溃或误报成功。
#
# 用法: bash tests/smoke.sh
# 环境变量:
#   BOOT_TIMEOUT  每项等待秒数（默认 45）
#   LOG_DIR       串口日志输出目录（默认 target/smoke-logs，CI 失败时据此上传）

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET="x86_64-unknown-uefi"
EFI="target/${TARGET}/debug/paranukos.efi"
BOOT_TIMEOUT="${BOOT_TIMEOUT:-45}"
LOG_DIR="${LOG_DIR:-${ROOT}/target/smoke-logs}"
mkdir -p "$LOG_DIR"

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

echo "==> 1/4 构建 EFI 镜像"
if ! cargo build --target "$TARGET"; then
    echo "[-] 构建失败。" >&2
    exit 1
fi
if [ ! -f "$EFI" ]; then
    echo "[-] 未找到构建产物: $EFI" >&2
    exit 1
fi
echo "    产物: $EFI"

echo "==> 2/4 编译测试用内核镜像（ET_EXEC）"
# 注意：x86_64-unknown-none 默认产出 PIE（ET_DYN），而引导器要求 ET_EXEC，
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
mkdir -p "$WORK/esp-extra/EFI/PARANUKO"
cp "$WORK/KERNEL.ELF" "$WORK/esp-extra/EFI/PARANUKO/KERNEL.ELF"

# 启动 QEMU 并轮询串口输出，直到匹配到正则或超时。
# 用法: boot_until <esp_extra|-> <日志文件> <正则>
boot_until() {
    local extra="$1" log="$2" pattern="$3"
    : >"$log"
    if [ "$extra" = "-" ]; then
        unset ESP_EXTRA
    else
        export ESP_EXTRA="$extra"
    fi

    ESP_DIR="$ESP_DIR" ./run-qemu.sh "$EFI" >"$log" 2>&1 &
    local pid=$!
    local waited=0
    while [ "$waited" -lt "$BOOT_TIMEOUT" ]; do
        if grep -qE "$pattern" "$log" 2>/dev/null; then
            kill "$pid" 2>/dev/null
            wait "$pid" 2>/dev/null
            return 0
        fi
        # run-qemu.sh 自己退出了（例如缺少 OVMF 固件），不必再等
        if ! kill -0 "$pid" 2>/dev/null; then
            break
        fi
        sleep 0.5
        waited=$((waited + 1))
    done
    kill "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
    return 1
}

show_log() {
    echo "    --- $1 串口输出（尾部） ---"
    tail -30 "$1" | sed 's/^/    /'
}

echo "==> 3/4 正向用例：ESP 中存在内核镜像"
if boot_until "$WORK/esp-extra" "$LOG_DIR/positive.log" '内核镜像校验通过'; then
    ok "QEMU 启动并完成内核镜像校验"
else
    bad "未在 ${BOOT_TIMEOUT}s 内观察到成功标志"
    show_log "$LOG_DIR/positive.log"
fi
if grep -q 'ELF64/x86-64 校验通过' "$LOG_DIR/positive.log"; then
    ok "ELF64/x86-64 头部校验通过"
else
    bad "缺少 ELF 头部校验日志"
fi
if grep -q '内核已载入 0x' "$LOG_DIR/positive.log"; then
    ok "内核镜像已复制到分配的物理页"
else
    bad "缺少内核载入日志"
fi

echo "==> 4/4 反向用例：ESP 中缺少内核镜像"
if boot_until "-" "$LOG_DIR/negative.log" '内核加载失败'; then
    ok "缺少内核镜像时优雅报错"
else
    bad "未观察到预期的错误日志"
    show_log "$LOG_DIR/negative.log"
fi
if grep -q '内核镜像校验通过' "$LOG_DIR/negative.log"; then
    bad "缺少内核镜像却报告成功"
else
    ok "未误报成功"
fi

echo
echo "结果: ${pass} 项通过, ${fail} 项失败"
if [ "$fail" -ne 0 ]; then
    exit 1
fi
exit 0
