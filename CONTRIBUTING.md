# 贡献说明

请先通过 Issue 描述复现步骤、操作系统、软件版本、预期与实际结果。使用匿名文件名和目录，不提交客户图片、凭据、真实数据库或完整本机路径。

一次 Pull Request 聚焦一个问题。涉及复制、覆盖、路径授权、凭据或网络行为时，请给出相应失败场景与回归测试。

提交前运行：

```sh
pnpm test
pnpm build
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo test --manifest-path src-tauri/Cargo.toml --locked
```

保留源文件不变、目标不静默覆盖、重号需人工确认的行为。不要为了测试通过删除安全检查，不要把模拟测试或进程存活表述为桌面操作完成。

严格 Clippy 的现存问题见 `docs/release-verification.md`。相关修复应单独说明，不在无关功能变更中批量重写。引入第三方资源或依赖时，核对许可证并更新 `THIRD_PARTY_NOTICES.md`。
