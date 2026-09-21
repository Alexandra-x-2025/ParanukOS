#!/bin/bash
# ParanukOS 专属 QEMU 加载脚本（方案1：直接以 UEFI kernel 方式加载）

# Cargo 会把编译好的 efi 路径作为第一个参数 ($1) 传给本脚本
EFI_PATH=$1

echo "[+] ParanukOS: Launching QEMU with EFI kernel at $EFI_PATH"

# 直接以 -kernel 方式加载编译产物（OVMF 自动识别 PE32+ UEFI application）
qemu-system-x86_64 \
    -bios /usr/share/edk2/ovmf/OVMF_CODE.fd \
    -net none \
    -nographic \
    -kernel "${EFI_PATH}"
