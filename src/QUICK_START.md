# OpenClaw 快速安装指南

## 一键安装

```bash
curl -fsSL https://raw.githubusercontent.com/A3S-Lab/Box/main/quick-install-openclaw.sh | bash
```

或者下载脚本后运行：

```bash
./quick-install-openclaw.sh
```

---

## 手动安装（3步完成）

### 步骤 1: 安装 A3S Box

```bash
brew tap a3s-lab/tap && brew install a3s-box
```

### 步骤 2: 安装 OpenClaw

```bash
mkdir -p ~/.openclaw/workspace && a3s-box pull ghcr.io/openclaw/openclaw:latest
```

### 步骤 3: 配置并启动

```bash
# 初始化配置
a3s-box run --name openclaw-onboard \
  -v ~/.openclaw:/home/node/.openclaw \
  -v ~/.openclaw/workspace:/home/node/.openclaw/workspace \
  ghcr.io/openclaw/openclaw:latest -- onboard && \
a3s-box rm openclaw-onboard

# 启动服务
a3s-box run -d --name openclaw-gateway \
  -p 18789:18789 \
  -v ~/.openclaw:/home/node/.openclaw \
  -v ~/.openclaw/workspace:/home/node/.openclaw/workspace \
  ghcr.io/openclaw/openclaw:latest
```

---

## 访问 OpenClaw

安装完成后，访问：**http://127.0.0.1:18789/**

---

## 常用命令

```bash
# 查看日志
a3s-box logs openclaw-gateway

# 停止服务
a3s-box stop openclaw-gateway

# 启动服务
a3s-box start openclaw-gateway

# 重启服务
a3s-box restart openclaw-gateway

# 进入容器
a3s-box exec -it openclaw-gateway -- /bin/bash
```

---

## 卸载

```bash
# 停止并删除容器
a3s-box stop openclaw-gateway && a3s-box rm openclaw-gateway

# 删除配置（可选）
rm -rf ~/.openclaw
```