#!/bin/bash
# OpenClaw 快速安装脚本 - 使用 A3S Box
# 只需三步即可完成部署

set -e

GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m'

echo -e "${BLUE}=========================================="
echo "OpenClaw 快速安装 (3步完成)"
echo -e "==========================================${NC}"
echo ""

# ============================================
# 步骤 1: 安装 A3S Box
# ============================================
step1_install_a3s_box() {
    echo -e "${BLUE}步骤 1: 安装 A3S Box${NC}"

    if command -v a3s-box &> /dev/null; then
        echo -e "${GREEN}✓ A3S Box 已安装${NC}"
        a3s-box --version
    else
        echo "正在安装 A3S Box..."
        brew tap a3s-lab/tap
        brew install a3s-box
        echo -e "${GREEN}✓ A3S Box 安装完成${NC}"
    fi
    echo ""
}

# ============================================
# 步骤 2: 安装 OpenClaw
# ============================================
step2_install_openclaw() {
    echo -e "${BLUE}步骤 2: 安装 OpenClaw${NC}"

    # 创建配置目录
    mkdir -p ~/.openclaw/workspace

    # 拉取并运行 OpenClaw
    echo "正在拉取 OpenClaw 镜像..."
    a3s-box pull ghcr.io/openclaw/openclaw:latest

    echo -e "${GREEN}✓ OpenClaw 安装完成${NC}"
    echo ""
}

# ============================================
# 步骤 3: 配置并启动 OpenClaw
# ============================================
step3_configure_openclaw() {
    echo -e "${BLUE}步骤 3: 配置并启动 OpenClaw${NC}"

    # 运行初始化配置
    echo "正在运行初始化配置..."
    a3s-box run --name openclaw-onboard \
        -v ~/.openclaw:/home/node/.openclaw \
        -v ~/.openclaw/workspace:/home/node/.openclaw/workspace \
        ghcr.io/openclaw/openclaw:latest \
        -- openclaw-cli onboard

    a3s-box rm openclaw-onboard 2>/dev/null || true

    # 启动 OpenClaw Gateway
    echo "正在启动 OpenClaw Gateway..."
    a3s-box run -d --name openclaw-gateway \
        -p 18789:18789 \
        -v ~/.openclaw:/home/node/.openclaw \
        -v ~/.openclaw/workspace:/home/node/.openclaw/workspace \
        ghcr.io/openclaw/openclaw:latest

    echo -e "${GREEN}✓ OpenClaw 配置完成并已启动${NC}"
    echo ""
}

# ============================================
# 执行安装
# ============================================
step1_install_a3s_box
step2_install_openclaw
step3_configure_openclaw

# ============================================
# 显示访问信息
# ============================================
echo -e "${GREEN}=========================================="
echo "安装完成！"
echo -e "==========================================${NC}"
echo ""
echo -e "${GREEN}访问地址:${NC}"
echo "  http://127.0.0.1:18789/"
echo ""
echo -e "${GREEN}常用命令:${NC}"
echo "  查看日志: a3s-box logs openclaw-gateway"
echo "  停止服务: a3s-box stop openclaw-gateway"
echo "  启动服务: a3s-box start openclaw-gateway"
echo ""
