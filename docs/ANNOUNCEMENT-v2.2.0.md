# A3S Box v2.2.0：把 v2.1.0 的能力焊死在正确性上

> 发布日期：2026-06-15 · 仓库：[AI45Lab/Box](https://github.com/AI45Lab/Box)

v2.1.0 带来了原生 snapshot-fork（虚拟机的隔离 + 容器的启动速度）。v2.2.0 不加新卖点，而是把已有能力**焊死在正确性上**：24 个修复，覆盖 CLI 状态机、运行时资源限制、guest-init 的 I/O、OCI 镜像存储、网络、温池（warm pool），以及 CRI 服务端。没有破坏性变更，CRI 一致性零回归。

一句话：**同样的能力，更经得起并发、异常路径和边界条件。**

---

## 为什么是一个纯加固版本

新特性容易展示，但真正决定一个运行时能不能托付生产的，是它在**并发、崩溃、异常路径**下的行为。v2.2.0 是一轮系统性的对抗式审计的产出——每一条都对应一个真实可复现的缺陷，并在 `/dev/kvm` 真机上用真实 OCI 镜像验证过。

---

## 修了什么

- **CLI 状态机**：`stop`/`start`/`kill`/`pause`/`unpause`/`rename`/`restart` 全部改走原子 `StateFile` 原语，关闭了「读-改-写」竞态对并发 box 状态的覆盖；`compose`/`snapshot` 的 box 注册改为原子（消除孤儿 VM 竞态）；`compose` 清理时先卸载 overlay 再删目录。
- **资源限制**：`--pids-limit` 在 `run` 路径上通过 guest 内 cgroup `pids.max` 真正生效；`resize` 加固——拒绝可注入的 cpuset 字符串、把 `cpu.weight` 钳到合法区间。
- **guest-init**：stdio 中继遇 `EINTR` 重试（容器输出不再被截断）；修复 cgroup 挂载 TOCTOU、stdio fd 泄漏、signal-64 边界；容器 `stdout`/`stderr` 可按路径重开（`/dev/stdout`、`/proc/self/fd/N`），让 Apache httpd 这类按路径重开日志的程序能正常启动。
- **exec**：exec 参数/环境变量做 base64 编码，shell 引号能安全穿过 libkrun 的环境传递。
- **OCI 存储**：拒绝存入摘要算法不可校验的 blob；store 与 build-cache 写入原子化（先暂存再 rename），并发 pull/build 不会损坏层。
- **rootfs 与网络**：overlay 逗号防护 + 有界卸载重试；单文件 bind mount 先暂存，让只共享目录的 virtio-fs 能服务它；passt 在启动超时时被杀、`terminate` 防 PID 复用、启动失败时回收 passt 以释放已发布端口。
- **温池**：snapshot-fork 不可用时回退到冷启动，而不是让池填充失败。
- **CRI**：维持 `StopPodSandbox` 状态不变量、为非运行容器正确上报 stats；流式错误路径上关闭 stdin 并发送 port-forward `CLOSE` 帧；拒绝空镜像引用；`RemoveImage` 解析镜像引用，使 `rmi <短标签>`（如 `alpine:latest`）可用；容器日志文件打开失败时上报而非吞掉。

---

## 一致性：零回归

在 `main` 上用真实 `crictl`/`critest` 重新跑了 Kubernetes CRI 一致性套件：

**73 通过 · 7 失败 · 17 跳过**（`critest` v1.30.1，跳过 portforward，80/97 specs）

通过数与既有基线一致——上述所有修复**零回归**。剩下的 7 个失败全部是 microVM 架构限制（mount 传播、宿主 namespace 共享、AppArmor enforce、非递归只读挂载），不是代码缺陷。详见 [`docs/cri-conformance.md`](./cri-conformance.md)。

---

## 升级

完全兼容 v2.1.0，无破坏性变更。直接升级即可：

```bash
brew upgrade a3s-box        # Homebrew
# 或从 GitHub Release 下载对应平台的 tarball
```

完整变更见 [CHANGELOG](../CHANGELOG.md)。
