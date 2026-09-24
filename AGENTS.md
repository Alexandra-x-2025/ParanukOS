# AGENTS.md - ParanukOS 协同开发协议

## 🛠️ 核心开发原则
* 每次改动完成后，都必须创建一个对应的 **Git commit**，以便后续追踪回滚。
* 每次改动后，都必须编写或更新相关测试，并在交付给用户前，确保所有测试和验证全部通过。

## 🚨 针对 ParanukOS (no_std / UEFI) 的追加规范（修改提示）
## 🚨 针对 ParanukOS (no_std / UEFI) 的追加规范（修改提示）
1. **防锁死终端条例（Exit QEMU gracefully）：**
   * 由于测试采用 `-nographic` 模式，验证成功后，AI 必须明确指示用户使用 `Ctrl + A` 继而按 `X` 来退出模拟器，严禁使用 `Ctrl + C`，防止僵尸 QEMU 进程在后台死锁并霸占物理串口。
2. **零幻觉依赖控制（No std crate pollution）：**
   * 在引入任何第三方 Crates 之前，必须对包进行审计，确保其带有 `default-features = false` 或原生支持 `#![no_std]`。严禁引入任何隐式依赖标准库的组件，否则编译链将直接崩溃。
3. **SELinux 状态感知（Fedora Host Environment）：**
   * 如果在构建验证阶段发生任何未知的硬件挂载错误（如 QEMU 提示无法读取文件），必须优先引导人类检查宿主机的 `setenforce` 状态，确保未受 Fedora 44 内核策略的静默拦截。
4. **多语言文档同步规范：**
   * **每次修改项目核心文档时，必须同时更新对应的中文版本（_CN 后缀文件）。** 确保中英双语信息的一致性。
