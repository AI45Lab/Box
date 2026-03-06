# 立即发布 a3s-box v0.8.0 到 winget

## 快速步骤

### 1. 等待 GitHub Release 完成

首先确认 v0.8.0 release 已经完成，Windows 资产已上传：
- 访问: https://github.com/A3S-Lab/Box/releases/tag/v0.8.0
- 确认存在: `a3s-box-v0.8.0-windows-x86_64.zip`

### 2. 计算 SHA256

```powershell
# 下载 Windows 发布资产
$Url = "https://github.com/A3S-Lab/Box/releases/download/v0.8.0/a3s-box-v0.8.0-windows-x86_64.zip"
Invoke-WebRequest -Uri $Url -OutFile "a3s-box-v0.8.0-windows-x86_64.zip"

# 计算 SHA256
$Hash = Get-FileHash -Path "a3s-box-v0.8.0-windows-x86_64.zip" -Algorithm SHA256
$SHA256 = $Hash.Hash
Write-Host "SHA256: $SHA256"
```

### 3. 更新 manifest 文件

编辑 `.winget/A3SLab.Box.installer.yaml`，将 `PLACEHOLDER_SHA256` 替换为实际的 SHA256 值。

### 4. 选择提交方式

#### 方式 A: 使用自动化脚本 (推荐)

```powershell
# 设置 GitHub token
$env:GITHUB_TOKEN = "your_github_token_here"

# 运行脚本
.\scripts\submit-to-winget.ps1 -Version "0.8.0"
```

#### 方式 B: 使用 GitHub Actions

1. 访问: https://github.com/A3S-Lab/Box/actions/workflows/publish-winget.yml
2. 点击 "Run workflow"
3. 输入版本: `0.8.0`
4. 点击 "Run workflow"

需要先配置 GitHub Secrets:
- `WINGET_TOKEN`: GitHub Personal Access Token
- `WINGET_FORK_USER`: 你的 GitHub 用户名

#### 方式 C: 手动提交

1. **Fork winget-pkgs 仓库**
   ```bash
   # 访问并 fork
   https://github.com/microsoft/winget-pkgs
   ```

2. **克隆你的 fork**
   ```bash
   git clone https://github.com/YOUR_USERNAME/winget-pkgs.git
   cd winget-pkgs
   ```

3. **创建分支**
   ```bash
   git checkout -b a3s-box-0.8.0
   ```

4. **创建 manifest 目录**
   ```bash
   mkdir -p manifests/a/A3SLab/Box/0.8.0
   ```

5. **复制 manifest 文件**
   ```bash
   # 从 a3s-box 仓库复制
   cp /path/to/Box/.winget/A3SLab.Box.yaml manifests/a/A3SLab/Box/0.8.0/
   cp /path/to/Box/.winget/A3SLab.Box.installer.yaml manifests/a/A3SLab/Box/0.8.0/
   cp /path/to/Box/.winget/A3SLab.Box.locale.en-US.yaml manifests/a/A3SLab/Box/0.8.0/
   ```

6. **验证 manifest**
   ```powershell
   # 下载 wingetcreate
   Invoke-WebRequest -Uri "https://aka.ms/wingetcreate/latest" -OutFile "wingetcreate.exe"

   # 验证
   .\wingetcreate.exe validate manifests/a/A3SLab/Box/0.8.0/
   ```

7. **提交并推送**
   ```bash
   git add manifests/a/A3SLab/Box/0.8.0/
   git commit -m "New package: A3SLab.Box version 0.8.0"
   git push origin a3s-box-0.8.0
   ```

8. **创建 PR**
   - 访问你的 fork: `https://github.com/YOUR_USERNAME/winget-pkgs`
   - 点击 "Compare & pull request"
   - 标题: `New package: A3SLab.Box version 0.8.0`
   - 描述:
     ```
     Add a3s-box v0.8.0 - MicroVM sandbox runtime with Windows WHPX backend support

     - Package: A3SLab.Box
     - Version: 0.8.0
     - Release: https://github.com/A3S-Lab/Box/releases/tag/v0.8.0

     This is a new package submission for a3s-box, a lightweight microVM sandbox
     runtime with Windows Hypervisor Platform (WHPX) backend support.
     ```
   - 创建 PR

### 5. 等待审核

- PR 提交后，winget 维护者会审核
- 通常需要 1-3 个工作日
- 关注 PR 评论，及时响应反馈

### 6. 验证发布

PR 合并后，用户可以安装：

```powershell
# 搜索
winget search a3s-box

# 安装
winget install A3SLab.Box

# 查看信息
winget show A3SLab.Box
```

## 注意事项

1. **SHA256 必须准确** - 从实际发布的文件计算，不能手动编造
2. **URL 必须可访问** - 确保 GitHub Release 已完成
3. **版本号一致** - 所有 manifest 文件中的版本号必须一致
4. **遵循命名规范** - PackageIdentifier 使用 `A3SLab.Box` (PascalCase)

## 故障排除

### 问题: SHA256 不匹配
**解决**: 重新下载文件并计算 SHA256，确保文件完整

### 问题: Manifest 验证失败
**解决**: 运行 `wingetcreate validate` 查看详细错误

### 问题: PR 被拒绝
**解决**: 查看 PR 评论，根据维护者反馈修改

## 后续版本

对于后续版本（如 0.8.1），可以使用 `wingetcreate update`:

```powershell
wingetcreate update A3SLab.Box `
  -v 0.8.1 `
  -u https://github.com/A3S-Lab/Box/releases/download/v0.8.1/a3s-box-v0.8.1-windows-x86_64.zip `
  -t YOUR_GITHUB_TOKEN
```

这会自动创建 PR 更新包。
