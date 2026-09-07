# GitHub Actions 编译与发布

推送到 `main` 或创建 Pull Request 时，`CI` 工作流会在 Windows x64 环境中执行：

1. 运行 `logd-core` 测试。
2. 编译 release 版 `logd.exe`。
3. 将可执行文件保存为 GitHub Actions artifact，保留 14 天。

## 发布版本

先更新根目录 `Cargo.toml` 中的 workspace 版本并提交，然后创建同版本标签：

```powershell
git tag v0.1.0
git push origin v0.1.0
```

`Release` 工作流会检查标签与 Cargo 版本是否一致，随后分别在 Windows 和 macOS 上构建、创建 GitHub Release，并直接上传：

- `logd.exe`
- `logd-0.1.0-windows-x86_64.msi`
- `logd-0.1.0-macos-<架构>.dmg`

这些是可直接下载的 Release 附件，不会再套一层 ZIP。当前未配置 Windows/macOS 代码签名，因此操作系统首次打开时可能显示安全提醒。

仓库的 Actions 设置必须允许 `GITHUB_TOKEN` 写入仓库内容。工作流已声明 `contents: write`，公开仓库和使用默认 token 权限的新仓库通常无需额外配置。
