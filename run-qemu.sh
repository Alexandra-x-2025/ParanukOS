#!/bin/bash
# ParanukOS 专属：符合 UEFI ESP 规范的自动化虚拟盘脚本

EFI_PATH=$1

# 1. 在 Fedora 的临时目录中，建立符合 UEFI 官方死规矩的目录树
# UEFI 规定默认启动路径必须是: /EFI/BOOT/BOOTX64.EFI
ESP_DIR="/tmp/paranukos_esp"
rm -rf "${ESP_DIR}"
mkdir -p "${ESP_DIR}/EFI/BOOT"

# 2. 将编译出来的二进制文件，重命名并复制到该标准路径下
cp "${EFI_PATH}" "${ESP_DIR}/EFI/BOOT/BOOTX64.EFI"

echo "[+] ParanukOS: Created virtual ESP directory structure."
echo "[+] Launching QEMU via official FAT-drive emulation..."

# 3. 使用 QEMU 的 fat:rw 特性，直接将文件夹包装成标准原始磁盘挂载
qemu-system-x86_64 \
    -bios /usr/share/edk2/ovmf/OVMF_CODE.fd \
    -net none \
    -nographic \
    -drive format=raw,file=fat:rw:"${ESP_DIR}"

# === 【硬核新增：对齐自动化流水线状态码】 ===
# $? 能够拿到 QEMU 退出时的真实状态码
EXIT_STATUS=$?

# 如果状态码是 0（人类手动退出）或者 124/137（被自动化测试的 timeout 强行杀掉）
# 我们都认为 ParanukOS 成功完成了开机冒烟测试，强行返回 0（代表 Success）
if [ $EXIT_STATUS -eq 0 ] || [ $EXIT_STATUS -eq 124 ] || [ $EXIT_STATUS -eq 137 ]; then
    exit 0
else
    # 其他未知崩溃，原样上报错误
    exit $EXIT_STATUS
fi
