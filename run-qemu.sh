#!/bin/bash
# ParanukOS 专属 QEMU 路径对齐脚本

# Cargo 会把编译好的 efi 路径作为第一个参数 ($1) 传给本脚本
EFI_PATH=$1

echo "[+] ParanukOS: Launching QEMU with firmware at $EFI_PATH"

# 完美拼接参数，调用 Fedora 44 的 UEFI 固件
qemu-system-x86_64 \
    -bios /usr/share/edk2/ovmf/OVMF_CODE.fd \
    -net none \
    -nographic \
    -drive format=raw,file="${EFI_PATH}"
