# 照片筛选助手 · Photo Selection Assistant

本地优先的桌面照片筛选与安全复制工具。它把客户选片文字或截图中的编号交给操作员确认，在源目录中递归匹配 RAW + JPG/JPEG，再复制到 `照片成片/待精修的原片`。匹配时忽略前导零，复制使用临时文件和 BLAKE3 校验，绝不静默覆盖源文件或内容不同的目标文件。

**当前版本：0.1.23，开发预览。** 自动测试通过不等于所有桌面和 NAS 场景已验收，详见 [验证记录](docs/release-verification.md)。

## 功能与使用

1. 创建会话，粘贴文字或导入截图；内置中文/英文离线 OCR。
2. 人工确认编号，选择源照片目录，递归匹配 RAW 与 JPG/JPEG。
3. 处理重号、多组候选、缺失与冲突，确认复制清单。
4. 安全复制并校验内容；相同内容跳过，不同内容的同名目标阻止复制。
5. 查看完成报告和历史记录；源文件不移动、不删除。

这个工具按已选编号找文件，不负责审美评分、自动选片、调色或修图。

```text
文字 / 截图 → 提取编号 → 人工确认 → 扫描 RAW + JPG
                                      ↓
完成报告 ← 内容校验与安全复制 ← 处理重号、缺失与冲突
```

详细操作请读 [使用说明](docs/user-guide.md)。

## 下载安装包

从 [GitHub Releases](https://github.com/muqeing/photo-selection-assistant/releases) 下载 0.1.23 预发布版：

| 系统 | 文件 |
| --- | --- |
| Mac Apple Silicon（M 系列） | `PhotoSelector-0.1.23-arm64.dmg` |
| Windows 64 位 | `PhotoSelector-0.1.23-x64-setup.exe` |
| Windows 64 位 MSI | `PhotoSelector-0.1.23-x64-zh-CN.msi` |

Release 同时提供 SHA-256 校验值、构建来源和第三方许可文件。Mac 包是本地 ad-hoc 签名、未 Apple 公证；Windows 包没有发布者签名。桌面全流程和实际 NAS 场景尚有待验收项，见验证记录。

## 从源码构建

本次验证环境：Node.js 22.23.1、pnpm 11.12.0、Rust 1.97.1、macOS 26.5.1（arm64）。Rust 元数据保留历史最低版本声明，当前依赖未重新验证旧编译器兼容性，建议使用上述已验证工具链。

Windows x64 需要 MSVC C++ Build Tools 和 WebView2；macOS 需要 Xcode Command Line Tools。参见 [Tauri 官方前置依赖](https://v2.tauri.app/start/prerequisites/)。

```bash
git clone https://github.com/muqeing/photo-selection-assistant.git
cd photo-selection-assistant
pnpm install --frozen-lockfile
pnpm prepare:ocr
pnpm tauri dev
```

首次安装依赖需要联网；默认 OCR 的运行资源随应用打包，不依赖 CDN。

## 检查与打包

```sh
pnpm test
pnpm build
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo test --manifest-path src-tauri/Cargo.toml --locked

# 发布打包要求 Git 工作区干净，在对应系统上执行
pnpm package:macos      # 在 Apple Silicon macOS 上执行
pnpm package:windows    # 在 Windows x64 + MSVC 上执行
```

`pnpm check:rust` 还包括严格 Clippy；当前 Rust 1.97.1 下存在已知规范和最低编译器版本告警，详见验证记录。macOS 包默认使用本地 ad-hoc 签名，不代表 Apple 公证；Windows 默认没有发布者签名。

包位于 `src-tauri/target/release/bundle/`，构建脚本生成来源提交与 SHA-256 记录。

`pnpm acceptance:fixture` 会在系统临时目录创建匿名的合成验收目录（不使用客户照片），包括前导零、RAW+JPG 和重号样本；命令输出 `acceptance-manifest.json` 中含源文件 SHA-256，方便在桌面端手动验收后核对源文件未变。

这些合成文件不能用于评估 RAW 解码、预览质量或真实 NAS 兼容性。

## 数据与云端调用

默认在本机进行 OCR、匹配和复制。只有主动配置并使用云端识别时，导入的识别内容才会发往指定服务商；连接测试也可能计费。密钥保存在系统凭据存储中。请勿把密钥、客户照片、真实数据库或未脱敏日志提交到仓库或 Issue。

配置方式详见 [模型服务商配置](docs/model-providers.md)。

## 开发与贡献

`src/` 为 TypeScript 界面、OCR 和 Tauri 桥接；`src-tauri/src/` 为 Rust 匹配、复制、存储及系统接口；`tests/` 和 `scripts/` 包含测试与构建工具。

欢迎提交可复现的问题和小范围修复，参见 [贡献说明](CONTRIBUTING.md)、[安全说明](SECURITY.md) 和 [第三方组件声明](THIRD_PARTY_NOTICES.md)。

## 许可证

本项目原创代码和图标以 [MIT License](LICENSE) 开源，Copyright (c) 2026 muqeing。第三方组件保留各自许可证，详见 [第三方声明](THIRD_PARTY_NOTICES.md)。
