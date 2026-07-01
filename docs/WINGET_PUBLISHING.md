# 发布到 Windows Package Manager (winget)

本文档说明如何将 a3s-box 发布到 Windows Package Manager (winget)。

## 方法 1: 自动发布 (推荐)

### 前提条件

1. **创建 GitHub Personal Access Token**
   - 访问 https://github.com/settings/tokens
   - 创建 token，权限: `public_repo`, `workflow`
   - 保存 token

2. **配置 GitHub Secrets**
   - 在仓库设置中添加 secrets:
     - `WINGET_TOKEN`: 你的 GitHub token
     - `WINGET_FORK_USER`: 你的 GitHub 用户名

### 触发发布

发布会在创建 GitHub Release 时自动触发，或手动运行：

```bash
# 在 GitHub Actions 页面手动触发
# Actions -> Publish to winget -> Run workflow
```

## 方法 2: 使用 PowerShell 脚本

```powershell
# 设置 GitHub token
$env:GITHUB_TOKEN = "your_github_token_here"

# 运行提交脚本
.\scripts\submit-to-winget.ps1 -Version "0.8.0"
```

脚本会自动：
1. 下载 Windows 发布资产
2. 计算 SHA256 哈希
3. 更新 manifest 文件
4. 验证 manifest
5. 创建 PR 到 microsoft/winget-pkgs

## 方法 3: 手动提交

### 步骤 1: 准备 manifest 文件

manifest 文件位于 `.winget/` 目录：
- `A3SLab.Box.yaml` - 版本清单
- `A3SLab.Box.installer.yaml` - 安装程序信息
- `A3SLab.Box.locale.en-US.yaml` - 本地化信息

### 步骤 2: 更新版本和 SHA256

```powershell
# 下载发布资产
$Version = "0.8.0"
$Tag = "v$Version"
$Url = "https://github.com/AI45Lab/Box/releases/download/$Tag/a3s-box-$Tag-windows-x86_64.zip"
Invoke-WebRequest -Uri $Url -OutFile "a3s-box.zip"

# 计算 SHA256
$Hash = Get-FileHash -Path "a3s-box.zip" -Algorithm SHA256
$SHA256 = $Hash.Hash
Write-Host "SHA256: $SHA256"
```

手动更新 `.winget/A3SLab.Box.installer.yaml`:
- `PackageVersion`: 更新为新版本
- `InstallerUrl`: 更新 URL
- `InstallerSha256`: 更新为计算的 SHA256
- `RelativeFilePath`: 更新路径中的版本号

### 步骤 3: 验证 manifest

```powershell
# 安装 wingetcreate
Invoke-WebRequest -Uri "https://aka.ms/wingetcreate/latest" -OutFile "wingetcreate.exe"

# 验证 manifest
.\wingetcreate.exe validate .winget\
```

### 步骤 4: 提交到 winget-pkgs

#### 选项 A: 使用 wingetcreate (推荐)

```powershell
.\wingetcreate.exe submit --token YOUR_GITHUB_TOKEN .winget\
```

#### 选项 B: 手动创建 PR

1. Fork https://github.com/microsoft/winget-pkgs
2. 创建目录: `manifests/a/A3SLab/Box/0.8.0/`
3. 复制 manifest 文件到该目录
4. 提交并创建 PR

## Manifest 文件说明

### A3SLab.Box.yaml (版本清单)
```yaml
PackageIdentifier: A3SLab.Box
PackageVersion: 0.8.0
DefaultLocale: en-US
ManifestType: version
ManifestVersion: 1.6.0
```

### A3SLab.Box.installer.yaml (安装程序)
```yaml
PackageIdentifier: A3SLab.Box
PackageVersion: 0.8.0
Platform:
- Windows.Desktop
MinimumOSVersion: 10.0.19041.0
InstallerType: zip
NestedInstallerType: portable
NestedInstallerFiles:
- RelativeFilePath: a3s-box-v0.8.0-windows-x86_64\a3s-box.exe
  PortableCommandAlias: a3s-box
- RelativeFilePath: a3s-box-v0.8.0-windows-x86_64\a3s-box-shim.exe
- RelativeFilePath: a3s-box-v0.8.0-windows-x86_64\a3s-box-guest-init
- RelativeFilePath: a3s-box-v0.8.0-windows-x86_64\lib\krun.dll
Installers:
- Architecture: x64
  InstallerUrl: https://github.com/AI45Lab/Box/releases/download/v0.8.0/a3s-box-v0.8.0-windows-x86_64.zip
  InstallerSha256: <COMPUTED_SHA256>
  Dependencies:
    WindowsFeatures:
    - HypervisorPlatform
ManifestType: installer
ManifestVersion: 1.6.0
```

### A3SLab.Box.locale.en-US.yaml (本地化)
包含包的描述、标签、发布说明等信息。

## 验证发布

发布成功后，用户可以通过以下命令安装：

```powershell
# 搜索包
winget search a3s-box

# 安装
winget install A3SLab.Box

# 升级
winget upgrade A3SLab.Box
```

## 注意事项

1. **Windows Feature 依赖**: Windows 包使用原生 WHPX 后端，不依赖 WSL。用户需要启用 Windows Hypervisor Platform：
   ```powershell
   Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform
   ```

2. **Portable 安装**: 使用 `portable` 类型，winget 会将 CLI 解压到用户目录并添加到 PATH。

3. **审核时间**: PR 提交后，winget 维护者会审核，通常需要 1-3 天。

4. **版本更新**: 每次发布新版本都需要提交新的 manifest。

## 故障排除

### SHA256 不匹配
确保下载的文件完整，重新计算 SHA256。

### Manifest 验证失败
运行 `wingetcreate validate` 查看详细错误信息。

### PR 被拒绝
查看 PR 评论，根据维护者反馈修改 manifest。

## 参考资料

- [winget 官方文档](https://learn.microsoft.com/en-us/windows/package-manager/)
- [winget-pkgs 仓库](https://github.com/microsoft/winget-pkgs)
- [wingetcreate 工具](https://github.com/microsoft/winget-create)
- [Manifest 规范](https://github.com/microsoft/winget-pkgs/tree/master/doc/manifest)
