# A3S Box v2.1.0：虚拟机的隔离，容器的启动速度

> 发布日期：2026-06-13 · 仓库：[AI45Lab/Box](https://github.com/AI45Lab/Box)

长期以来，运行不可信代码只有两个糟糕的选择：**容器**快、密度高，但和宿主机共享同一个内核，隔离薄弱；**虚拟机**有自己的内核、隔离过硬，却启动慢、扛不住规模化。A3S Box v2.1.0 把这个取舍彻底拆掉了——让每个工作负载拥有**自己的真实内核**（虚拟机级别的隔离），同时获得**容器级别的启动速度与密度**。

本次发布的核心是 **原生 snapshot-fork（写时复制的 microVM 克隆）**，再加上你已经熟悉的 Docker 式体验。

一句话：**虚拟机的隔离，容器的启动与密度，Docker 的手感。**

---

## A3S Box 是什么

A3S Box 是一个类 Docker 的 MicroVM 运行时（开源，仓库 `AI45Lab/Box`）。它把每个 Linux OCI 工作负载跑在自己的 libkrun MicroVM 里（**一个 box 一个真实内核**），并提供：

- Docker 式 CLI（`run` / `build` / `exec` / `logs` / `compose`）
- OCI 镜像存储
- Kubernetes CRI 接入
- 可选的 AMD SEV-SNP 机密计算（TEE）

---

## 取舍是怎么被拆掉的

容器与虚拟机的取舍，根子在于「内核」与「启动成本」绑死了：要独立内核，就得忍受整机冷启动。

snapshot-fork 把这两件事解耦，核心思路只有两步：**快照一台已经启动好的模板 VM，然后把它 CoW 地分叉成很多份。**

**1. 快照（capture）一台已启动的模板**

先正常冷启动一台模板 VM，让它进入「已就绪」状态，然后把它运行时的完整状态落盘：

- 文件背书的 guest RAM（file-backed guest memory）
- KVM vCPU 状态
- virtio 设备状态

这三者一起，构成了一份「开机后即可恢复」的完整快照。

**2. 恢复（restore）时按 `MAP_PRIVATE` 分叉**

每一个分叉副本在恢复时，把模板的那份 guest RAM 以 `MAP_PRIVATE` 方式映射进来。于是：

- 所有分叉共享同一份只读的模板内存页；
- 每个分叉**只为自己改写（dirty）过的页付费**——写时复制，按需分配。

于是「一个工作负载一个真实内核」不再意味着「一次整机冷启动」。隔离照旧是虚拟机级别的，启动成本却塌缩到了容器级别——这就是 A3S Box 同时拿到 VM 级隔离与容器级启动/密度的原理。

### 怎么用

快照分叉由几个环境变量驱动，也可以走 per-VM 的 `BoxConfig`：

- 抓取快照：`KRUN_SNAPSHOT_MEM_FILE` / `KRUN_SNAPSHOT_SOCK`
- 从快照恢复：`KRUN_RESTORE_FROM`
- 或者直接用每台 VM 的 snapshot/restore 配置接缝（per-VM config seam）

暖池（warm pool）也接上了这条路径：

```bash
# 冷启动一台模板，其余全部 CoW 恢复（并发填充）
a3s-box pool start --snapshot-fork
```

---

## v2.1.0 新增能力

- **原生 snapshot-fork（CoW microVM 克隆）**：通过 `KRUN_SNAPSHOT_MEM_FILE` / `KRUN_SNAPSHOT_SOCK`（捕获）+ `KRUN_RESTORE_FROM`（恢复）驱动，或使用按 VM 的 `BoxConfig` 配置。
- **预热池 `pool start --snapshot-fork`**：冷启动一个模板，其余的用 CoW 恢复（支持并发填充）。
- **`prune` 命令**（别名 `container-prune`）：清除所有已创建 / 已停止 / 已死亡的 box，对齐 Docker 的 container prune。
- **按 VM 的 snapshot/restore 配置接缝**。
- **修复：原子化的并发 box 注册**——100 个 box 并发启动，零记录丢失（此前存在丢失更新竞态 + O(N²) 问题）。
- **跨平台可移植性加固**，使 snapshot-fork 能在 linux-arm64 与 Windows 上构建。

---

## 实测数据

以下数字均为 **`/dev/kvm`（Linux KVM）宿主机实测**，并已在恢复出的 guest 内通过 virtio-fs 真实执行（real exec）验证。这些数字**依赖具体宿主机，是实测值，而非普适保证**。

| 场景 | 结果 | 对比 |
|------|------|------|
| 单 VM 冷启动 | 约 200 ms（`/dev/kvm` 实测） | 基线 |
| snapshot-fork 单个 fork | 约 110 ms（`/dev/kvm` 实测） | 比冷启动（约 450 ms）快约 4× |
| 100 个 fork | 总计 < 约 1 s（`/dev/kvm` 实测） | 每 VM 摊销约 8 ms，单个约 13 MB RSS，0 错误，全部可 exec |
| 预热池服务（已预启动） | 约 73 ms（`/dev/kvm` 实测） | 相对冷启动（约 1688 ms）约 23× |
| 池填充到 8 个 | 约 12.4 s → 约 1.9 s（`/dev/kvm` 实测） | 启用 `--snapshot-fork` + 并发填充 |

---

## 适合的场景

### 1. AI Agent 沙箱舰队

让一个 agent 跑不可信代码时，你需要的是**强隔离**而不是共享内核的容器。过去要隔离就得用 VM，而 VM 启动慢、密度低，没法为每一次工具调用、每一个并发会话临时拉起一个。

现在不一样了：先冷启动**一台**模板 VM，再用 CoW 快照分叉把其余沙箱「印」出来——单个分叉**约 110 ms**，100 个分叉在**约 1 秒内**全部就绪，**每个 VM 摊销约 8 ms**、**约 13 MB RSS**、0 错误，且每一个都能真实 exec（`/dev/kvm` 实测，已在还原后的 guest 内经 virtio-fs 验证真实执行）。海量、短命、互相隔离——正是 agent 平台需要的形态。

### 2. 多租户不可信代码执行

把陌生用户提交的代码放进共享内核的容器里，始终是一道横在心头的安全题。A3S Box 给每个租户、每次执行一个**真实内核**（VM 级隔离），而代价不再是 VM 的启动开销。配合 `prune`（别名 `container-prune`）一键清理所有 created/stopped/dead 的 box，回收快、收口干净，适合「拉起—执行—销毁」的高频循环。

### 3. 快速而安全的 CI

CI 任务彼此之间、与宿主之间都该被隔离开。用预热池把环境提前备好，任务到来时直接服务——预热池服务（已预启动）**约 73 ms**，相对冷启动（约 1688 ms）**约 23 倍**；池填充到 8 个从**约 12.4 秒**降到**约 1.9 秒**（开启 `--snapshot-fork` + 并发填充，均为 `/dev/kvm` 实测）。每个流水线步骤都跑在独立内核里，既快又稳。

### 4. 机密计算

可选的 AMD SEV-SNP（TEE）让工作负载在加密、可证明的环境中运行，适合对数据机密性有硬性要求的场景。

---

## 诚实的边界

我们不夸大。这是这个产品的文化：

- **数字是宿主机相关的。** 上面所有数据都是特定 `/dev/kvm` 宿主机上的**实测值**，不是理想化的目标值，也不是普适保证。
- **「100 个 VM 共 20 ms」并未达成。** 在一台 8 核机器上做不到——那需要一个 fork 守护进程外加 32+ 核。但**每 VM 摊销约 8 ms** 这一点，已经胜过同类的 forking VMM。
- **A3S Box 不是 Docker / containerd / Kubernetes 的完整替代品。** 本地 CLI 运行时是**主要、最完整**的使用面；CRI / TEE / Windows 都是真实存在的能力，但需要在你的宿主机上做验证。

我们的态度很简单：宁可把边界讲清楚，也不替你许下你机器兑现不了的承诺。

---

## 上手

安装（macOS / Linux）：

```bash
brew install a3s-lab/tap/a3s-box
```

在任意宿主机上的第一条命令——它会报告虚拟化能力 / 平台 / TEE 支持：

```bash
a3s-box info
```

---

## 链接

- **发布页**：https://github.com/AI45Lab/Box/releases/tag/v2.1.0
- **仓库**：https://github.com/AI45Lab/Box

虚拟机的隔离，容器的速度。现在就 `brew install a3s-lab/tap/a3s-box`，跑一条 `a3s-box info` 看看你的宿主机能解锁什么。
