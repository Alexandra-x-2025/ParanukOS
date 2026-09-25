# ParanukOS 贡献指南

[English](CONTRIBUTING.md) | [中文]

欢迎加入 **ParanukOS** 项目！我们非常高兴你能参与构建这个基于 Rust 的高可靠性微内核操作系统。

通过为本项目做出贡献，你正在帮助我们建立一个安全、稳定且可扩展的计算基础。在提交任何代码之前，请务必仔细阅读本指南。

## 📜 核心原则
在编写代码之前，请确保你的工作符合我们的核心哲学：
*   **隔离优先 (Isolation First):** 所有组件（驱动程序、文件系统等）必须尽可能运行在用户态并相互隔离。
*   **微内核纯粹性 (Microkernel Purity):** 内核应仅负责最基础的任务，如 IPC、内存管理和基本调度。
*   **Rust 安全性:** 我们利用 Rust 来消除内存安全漏洞。严格遵守 `#![no_std]` 约束是强制性的。

## 🛠️ 开发环境
为了高效贡献，你需要准备以下工具：
*   **Rust 工具链:** Rust **stable** —— 已在 1.98 上验证，当前**不需要** nightly。
*   **目标平台:** `x86_64-unknown-uefi`（引导器本身）与 `x86_64-unknown-none`（冒烟测试用来编译占位内核镜像）。
*   **QEMU 与 OVMF:** 用于运行和测试镜像。
*   **构建工具:** 确保已安装 `cargo` 和相关的编译依赖。

### 环境配置
1.  克隆仓库: `git clone <repo_url>`
2.  添加目标平台: `rustup target add x86_64-unknown-uefi x86_64-unknown-none`
3.  安装 QEMU 与 OVMF: `sudo apt install qemu-system-x86 ovmf`（Debian/Ubuntu）或 `sudo dnf install qemu-system-x86 edk2-ovmf`（Fedora/RHEL）。

构建、运行与测试命令见 [README_CN.md](README_CN.md)。

## 💻 代码规范
*   **语言:** 所有核心系统代码必须使用 Rust 编写。
*   **无标准库约束:** 内核空间代码必须严格遵循 `#![no_std]`。
*   **内存安全:** 除非为了直接操作硬件而绝对必要，否则应避免使用 `unsafe` 代码块。如果必须使用，请提供详细的注释解释其安全性。
*   **IPC 协议:** 所有进程间通信必须遵循项目定义的 IPC 消息格式和能力令牌（Capability Tokens）。

## 🧪 测试与质量保证
我们通过严格的验证来维持系统的可靠性。在提交 PR 之前，请确保以下各项全部通过 —— 这几项也正是 CI 所执行的：

1.  **静态检查:** `cargo fmt --all -- --check` 与 `cargo clippy --all-targets -- -D warnings`。
2.  **镜像校验:** `python3 tests/check_pe.py target/x86_64-unknown-uefi/debug/paranukos.efi`，从 PE 结构层面确认产物是可加载的 PE32+ EFI 应用（而不是匹配 `file(1)` 的输出措辞）。
3.  **引导测试:** `bash tests/smoke.sh` 在 QEMU/OVMF 下引导镜像，同时断言成功路径（找到合法内核镜像、校验并载入）与失败路径（缺少内核镜像时优雅报错）。
4.  **单元测试:** 该 UEFI 二进制必须保留 `[[bin]] test = false` —— 基于 `std` 的测试框架会与 UEFI 的 panic 处理器冲突，报重复的 `panic_impl` lang item 而编译失败。因此值得单元测试的逻辑应放入不依赖其他 crate 的模块或独立 crate，以便针对宿主平台构建与测试。
5.  **文档更新:** 任何新的功能或重大的架构变动必须同步更新 `docs/architecture/` 下的文档；按 `AGENTS.md` 的要求，还需同步对应的中文（`_CN`）版本。

## 🚀 拉取请求 (Pull Request) 流程
1.  **Fork 与分支:** 从 `main` 分支创建一个功能分支。
2.  **提交信息:** 使用清晰、描述性的提交信息（例如：`feat(ipc): 添加零拷贝消息传递`）。
3.  **一个 PR 只做一件事:** 保持改动聚焦，便于独立审查与回滚。
4.  **文档同步:** 确保所有新 API 已在项目文档中记录。
5.  **代码审查:** 提交 PR 后，我们将重点审查以下内容：
    *   内存安全性及 `unsafe` 代码的使用合理性。
    *   是否符合微内核的隔离原则。
    *   IPC 调用对系统性能的影响。

## 🤝 沟通与协作
我们非常重视清晰的沟通。如果你遇到技术障碍或有架构上的疑问，请在开始大规模开发前通过项目的指定渠道进行讨论。

---
*感谢你帮助我们共同构建更稳定、更安全的 ParanukOS！*
