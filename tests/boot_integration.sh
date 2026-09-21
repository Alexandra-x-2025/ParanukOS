#!/bin/bash
# Integration smoke test for the cargo UEFI boot runner (.cargo/config.toml).
#
# 验证：`cargo run --release` 会经由 QEMU + OVMF/EDK2 成功引导 ParanukOS。
# 由于内核进入死循环，QEMU 会被 timeout 杀掉（exit=124）——这与 run-qemu.sh 的语义一致：
#   exit 0 / 124 / 137 均视为“开机冒烟测试通过”。
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

CONFIG="$ROOT/.cargo/config.toml"
EFI="target/x86_64-unknown-uefi/release/paranukos.efi"
BANNER='ParanukOS Next-Gen Kernel'

fail() { echo "[FAIL] $*"; exit 1; }

echo "[+] [1/4] config.toml exists and defines a UEFI runner ..."
[ -f "$CONFIG" ] || fail "missing $CONFIG"
grep -q 'x86_64-unknown-uefi' "$CONFIG" || fail "config missing x86_64-unknown-uefi target"
grep -q 'runner = .*qemu-system-x86_64' "$CONFIG" || fail "config missing qemu runner"

echo "[+] [2/4] OVMF firmware is available ..."
[ -f /usr/share/edk2/ovmf/OVMF_CODE.fd ] || fail "OVMF_CODE.fd not found at /usr/share/edk2/ovmf/"

echo "[+] [3/4] build ParanukOS EFI image (release) ..."
cargo build --release >/dev/null 2>&1 || fail "cargo build --release failed"
[ -f "$EFI" ] || fail "expected $EFI after build"

echo "[+] [4/4] boot via cargo runner; expect banner + success exit code ..."
# timeout=15：内核死循环会挂起，timeout 杀掉 QEMU（exit=124）即代表成功引导。
set +e
OUT="$(timeout 15 cargo run --release 2>&1)"
CODE=$?
set -e

echo "$OUT" | grep -q "$BANNER" || fail "boot banner '$BANNER' not found in QEMU output"

# 0 = 正常退出；124/137 = 被 timeout 杀掉（引导成功后的预期行为）
if [ "$CODE" -ne 0 ] && [ "$CODE" -ne 124 ] && [ "$CODE" -ne 137 ]; then
    fail "unexpected exit code $CODE (expected 0/124/137)"
fi

echo "[OK] ParanukOS booted successfully via cargo UEFI runner (exit=$CODE)."
