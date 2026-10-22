# pVisor VM 内存优化路线图

> 本文件是 `feature/memory_opt` 工作分支的临时路线图，不属于在线文档。
> 合并到 `main` 后删除本文件；如果需要长期保留，应另行决定归档位置。

## 目标

当前目标是：**pVisor VM 启动后在短时间内达到稳态，并降低稳态内存占用。**
P0 先在本机固定输入、实验环境和测量协议，建立只包含 `pvisor-*` 的 baseline，
后续优化都与该 baseline 对照，同时保持内容正确性、写入隔离和恢复语义。

主要指标是**启动到稳态的时间、稳态内存水平与波动范围**；同时记录启动峰值、
CPU 成本、读回和写入后的内存变化。更快回收和更低最终占用分别判断，不能相互替代。
使用完整 VM 组（含池服务）的计费内存及 PSS，不以 RSS 相加、压缩字节或逻辑回收量代替。

指标优先级：稳态时间、占用和波动是主要目标；回收曲线与“持续减少 128 MiB”
解释过渡阶段，不代替稳态验收；CPU、启动峰值和访问后回升用于评估优化代价。
计费内存是主要占用口径，PSS 用于交叉检查共享与驻留变化。

提高同一节点预算下的有效 sandbox 密度是后续应用目标，不是 P0 的验收前提。
当前不要求 Firecracker/Docker 外部对照、16/32 VM 密度或 resume P95。
所有对外结论必须来自已登记的 benchmark cohort；本机 N=1 预检和机制探针只用于工程判断。

## 当前基线与缺口

pVisor 已有完整的内存机制栈：

- 快照基线 + 私有 COW、按需 RAM 恢复和增量 RAM 捕获；
- Linux KSM 建议、daemon 物理页池、密封压缩池、本地冷页压缩；
- 整机 RAM offload、free-page reporting、guest 文件缓存回写；
- `environment_snapshot`、`capture_ram_with_delta` 和 `restore_ram` 的 base/delta 原语。

但默认路径仍有明显缺口：

- Linux 静态构建的内核和 guest 初始化内容按 VM 复制，没有跨 VM 共享；
- daemon 默认使用每 sandbox 独立的 `live-ram` 文件，未开启共享；
- `pause` 只停 vCPU，不回收内存，也不释放逻辑内存配额；
- 普通创建不走模板/快照 COW 基线，只有 checkpoint/fork 路径支持；
- immutable rootfs/库的 DAX 默认关闭，guest page cache 仍逐 VM 复制；
- 准入按逻辑上限求和，共享页、私有页、冷页和暂停保留没有分开记账；
- 高级机制互斥且默认关闭，没有节点级策略决定何时扫描、压缩或卸载。

因此先建立**本机启动到稳态的可重复基线**，按早期下降、稳态占用及访问后回升定位优化点。
后续模板/不可变页共享、扫描策略和压缩优化均使用相同口径评估；节点记账与暂停生命周期
负责把节省转化为可用容量，不作为建立 P0 baseline 的前置条件。

### 历史数据的参考范围

[内存性能分析](docs/src/zh/lab-notes/vm-memory-performance-analysis.md)中的历史实验
用于理解机制：物理池较早降低占用，KSM 收敛较慢但最终占用可能更低；压缩后的空闲
低位可能在读回时明显回升；文件提示主要影响早期回收，共享也可能在 Ready 前发生。

历史曲线从全部 VM Ready 开始计时，“持续减少 128 MiB”衡量回收速度，
“60 秒占用”只是窗口末尾读数，都不能直接回答启动后多久稳定。
这些 N=1 数据只作趋势参考；后续优化以 P0 的本机启动至稳态 baseline 为对照。

## 总体架构

```text
L4 工作负载信号：Agent Runtime / Gateway model wait / daemon idle
L3 节点内存管理：分类、记账、准入、配额释放、压力策略
L2 生命周期：pause / offload / checkpoint / suspend / resume
L1 基线共享：Golden Snapshot COW + kernel/rootfs 不可变页
L0 页级机制：UKSM/物理池扫描、冷页压缩、free-page reporting
```

各层职责：

- **L0** 只负责减少重复页或压缩冷页，不决定是否值得做；
- **L1** 复用启动后的初始内存和不可变内容，收益上限最高；
- **L2** 在保留执行状态的前提下降低驻留内存，并提供恢复路径；
- **L3** 把物理节省转化为可准入容量，并约束失败域；
- **L4** 只提供“可以等待/可以暂停”的提示，最终由 L3/L2 验证并执行。

### 参考实现

以下只作为方法参考，不作为本项目的复测结论：

- **CubeSandbox**：模板包含启动后的内存快照；CubeCoW 用 reflink 做内存/rootfs 克隆；
  soft-dirty 增量快照减少重复写；AutoPause 释放实例并按比例归还配额。
- **OpenSandbox Fast Sandbox**：Firecracker 模板恢复 `vmstate.snap`；Fastlet 预热池
  承载多个 sandbox；pause 将完整 checkpoint 写入 artifact store 并释放 Fastlet 容量。
- 两者都更依赖**模板基线共享 + 暂停释放生命周期**，而不是运行期跨 VM 页去重；
  pVisor 的页级机制更丰富，但需要补上同样的生命周期和节点记账。

## P0：建立本机启动至稳态基线

回答两个问题：**当前各 pVisor 模式启动后多久稳定，稳定时占多少内存？**
本阶段固定环境、输入和测量协议，交付可复查的 baseline，作为后续优化的起点。
当前基线采集与交付已完成；与优化实现的受控对照尚未完成，不能据此宣称优化目标已经达成。

### 实验与判定口径

- 复用 `B-MEMORY-SCALE` 的四 VM 协议，只观察 `pvisor-*`。
  `full` 包含 off/local/physical/ksm/compressed 五模式 ×
  repeated/random-shared/random-unique 三种内容，共 15 条件；
  `quick` 包含 off/physical/ksm × 前两种内容，共 6 条件。
  full/quick 是实验配置，不是内存机制，图中不存在 `pvisor-quick` 曲线。
- 固定 4×256 MiB VM，每 VM 1 vCPU、64 MiB payload；整组四核、2 GiB、禁用 swap，
  计入 VM、池服务及组内控制进程。依次启动，全部 Ready 后空闲 60 秒，
  再完整读回、进行 25%/100% 私有写入并观察写后 30 秒。
- 从产品组启动前采样，标记启动、逐 VM Ready、读回和写入。
  整组计费内存每 50 ms、进程 PSS 每 500 ms 采样；计费内存为主指标，PSS 辅助解释共享。
- **稳态判据**：在全部 Ready 后的 60 秒空闲窗口中，寻找最早时刻 t，
  使 t 至窗口结束至少有 10 秒观测，且该后缀的计费内存极差不超过
  `max(4 MiB, 后缀中位数的 2%)`。报告启动至 t、Ready 至 t 的时间，
  以及稳态内存中位数和最小/最大值。未满足则明确标记“窗口内未达稳态”；
  采样不足则标记“证据不足”，不以 60 秒读数冒充稳态。
- 同时保留启动峰值、回收过程、CPU 成本、读回耗时和访问后的内存回升；
  写后使用同一稳态判据。内容正确性、跨 VM 写入隔离、独立退出及清理检查作为有效性门禁。
- 测量使用冻结输入，不在采集期间构建或修改 harness；记录源码、输入、判据及宿主配置，
  在测量期间检查一致性，最终校验通过后才发布成功状态。宿主权限与 KSM 由管理员预置，
  脚本只检查和记录，不自动修改。详细协议见
  [baseline 说明](benchmark.new/memory/README.md)。

### 本机基线结果（2026-10-22）

已完成完整 full 和独立 quick 复测，详见[数据与曲线](benchmark.new/memory/LOCAL_BASELINE.md)：

- [full-20261022-1](target/tmp/full-20261022-1/baseline.json)：15/15 条件、870 项内容/生命周期检查通过；
  [quick-20261022-1](target/tmp/quick-20261022-1/baseline.json)：6/6 条件、348 项检查通过。
  两批均为 `complete=true`、`evidence_valid=true`，无 OOM/swap/配额失败，专属组已清理。
- 相同随机内容中，物理池在启动后约 **4.86 秒**进入约 **229.96 MiB** 稳态，
  KSM 约 **35.81 秒、180.18 MiB**；off 在 Ready 时已稳定，约 **405.44 MiB**。
  物理池更早稳定，KSM 稳态占用更低，后续优化需要同时衡量这两个目标。
- 本地压缩与压缩池的六个空闲条件均在 60 秒内未达稳态判据。
  重复内容读回后，两者分别回升至 378.69 / 384.18 MiB；
  物理池相同随机内容在全量私有写入后稳态为 437.07 MiB。
  空闲节省不能直接外推为访问后的占用或节点容量。
- quick 的六个重叠条件再次达到稳态；两轮稳态占用差最大 0.875 MiB，
  Ready 至稳态时间差最大约 0.240 秒。每条件每批 N=1，仅用于本机工程判断，
  不合并批次，不提供置信区间或生产密度结论。以上内存均为四 VM 完整组占用。

运行记录保存为真实目录 `target/tmp/full-日期-序号/`、`target/tmp/quick-日期-序号/`，
各模式按 UTC 日期独立递增；共用冻结输入现位于 `target/tmp/inputs/`。
原始测量记录中的执行路径保留历史值。采集期间 KSM 为 `run=1`，结束后已恢复原 `run=0`。

### 已完成与待完成事项

- [x] 固定实验协议、环境记录和稳态判据，交付 full 基线及原始数据、曲线与摘要。
- [x] 独立运行 quick 复测，完成正确性、环境及来源校验。
- [ ] 沿用本机 baseline 的负载、资源预算、环境配置和稳态判据；保留原始基线，
      新旧实现受控对照后再报告改善比例。修改判据时须另存分析结果，并重算可比原始数据。
- [ ] 先用 quick 检查迭代趋势；涉及压缩或独有页时覆盖相应 full 条件。
      同时报告稳态耗时、占用、波动及 CPU/峰值/读回代价，明确尚未达稳态的条件。

共享库/DAX、正式多轮、外部产品及高密度实验属于后续扩展，另立补充协议，
不作为当前 P0 的待完成验收项。

## P1：节点内存管理与暂停回收

这是把页级节省变成 daemon 并发数的前提，也是与现有 `RunOffload` 对接的最小闭环。

- [ ] daemon 准入区分 Running / Paused；暂停实例按经过验证的回收结果释放
      可配置比例的内存配额，而不是直接按逻辑值打折。
- [ ] resume 前重新做节点准入；容量不足时返回明确错误并保持 Paused，
      不能先恢复再依赖 OOM killer。
- [ ] 把 pause 抽象成可插拔策略：
      - 当前 file-backed shared RAM：`RunOffload`；
      - 未来模板私有 COW：`RunCheckpoint`/`RunSuspend` 或 base + delta capture；
      - 不允许把 `RunOffload` 硬编码成唯一实现。
- [ ] opt-in idle auto-pause：基于数据面活动、显式生命周期和可配置超时；
      Agent 不合作时仍能回收空闲实例。
- [ ] 物理池、模板缓存和压缩池预算独立于 sandbox cgroup，并计入节点预算；
      为池/模板/恢复峰值保留 headroom。
- [ ] 保持手动 pause/resume 与自动 pause 的优先级和 epoch 语义；
      自动恢复不能覆盖手动暂停。

**验收：** 暂停实例确实降低 `memory.current`；resume 不产生 OOM；
暂停/恢复内容、文件 I/O 和状态正确；配额释放比例可配置且有上限；
`RunOffload` 之外的恢复路径有明确 capability 拒绝。

## P2：Golden Snapshot 模板与不可变页共享

借鉴 CubeSandbox 的 Golden Snapshot 思路：从已启动并达到就绪状态的 VM 捕获模板，
新 sandbox 从模板内存基线恢复，未修改页共享，写入产生私有 COW。
pVisor 已有 `environment_snapshot`、`restore_ram` 和增量捕获原语，缺的是普通创建和 daemon 路径。

- [ ] 先做 Linux x86_64、2 VM、无网络或 daemon 兼容 profile 的正确性探针：
      共享基线、私有写入、文件 I/O、退出和引用回收。
- [ ] 定义模板身份、兼容性、版本、生命周期和 GC；模板必须绑定
      binary、firmware、kernel、rootfs 和 guest runtime 版本。
- [ ] 将模板恢复接入 daemon create；为每个实例分配独立身份、凭据、
      控制连接、可写 upper 和随机状态。
- [ ] 处理网络、vsock、prepared rootfs、writable backing 和 snapshot profile
      的兼容性；不能静默降级为“看起来共享、实际独立复制”。
- [ ] 把 kernel sharing 作为第一步验证：共享只读 kernel 映射 +
      `MAP_PRIVATE` COW，避免静态 Linux 每个 runner 的匿名复制和 RAM 文件复制。
- [ ] 实现模板模式的暂停：只捕获相对基线的私有 COW 页（base + delta），
      不把完整 RAM 重新写一遍；恢复时重新映射 base + delta。
- [ ] 验证模板内容不含租户密钥；跨信任域默认不共享模板，除非显式授权。

**验收：** 2 VM 内容/私有写入/文件 I/O 正确；按 P0 的四 VM 固定协议比较模板与独立启动，
报告启动到稳态时间、稳态占用、CPU 和峰值；16/32 VM 密度须另行登记，不外推四 VM 结果。
模板创建、恢复失败、GC 和 daemon 重启均有明确状态；
私有 COW 暂停走 delta 路径，而不是被 `RunOffload` 拒绝。

## P3：UKSM/物理池启发式扫描

当前 `SharedPool` 已有 UKSM 风格的 region distilling。本阶段的目标不是再写一个扫描器，
而是让扫描更懂 Agent VM 的重复模式和生命周期。

- [ ] 对齐现有 adaptive scanner：region 分级、promotion/demotion、COW churn、
      8 ms 软预算和 250 ms 轮转；新增策略作为候选选择增量。
- [ ] 利用 Agent VM 特征：相同镜像/运行时/工具链、零页、只读库页、
      启动/模型等待/工具执行阶段的重复率差异。
- [ ] 对刚 COW、刚写入、刚被设备访问的页设置冷却；跳过文件页、DAX、
      hugepage 和不可共享映射。
- [ ] 若 VM 即将 offload 或进入模板恢复，停止无意义的扫描。
- [ ] 以“每节省 1 MiB 的扫描 CPU、COW break 率、恢复尾延迟”评估，
      不以 merge 页数作为唯一指标。
- [ ] 在 P2 之后重新评估边际收益：模板已共享的页不应再次扫描；
      只有残余私有 COW 和不同模板之间的重复才值得处理。

**验收：** 在固定重复率/写入率负载上，扫描 CPU 或尾延迟不劣化，
且相对 P2 基线有可测的额外节省；没有达到阈值的启发式保持 opt-in。

## P4：Agent 等待协作与按需卸载

`NativeModelWait::Pause` / `Offload` 已有 M0 实现，但默认关闭、仅 Gateway 场景、
没有 CLI/配置开关，且仍保留资源准入。本阶段把它产品化，而不是重新设计。

- [ ] 把 model wait 的 pause/offload 从 embedded-only 变成明确配置；
      保留默认关闭和平台 capability 拒绝。
- [ ] 完成 resume 前的资源重准入；模型响应到达后先恢复资源，再交付 guest。
- [ ] 把 Guest/Agent 的“长时间等待”作为 hint，由 daemon 验证
      所有可运行活动、脏页和设备引用都已静止后才执行回收。
- [ ] 普通下载、磁盘 I/O 和工具等待走 EXP-003 的 host-owned I/O / spool 路径；
      不能把“一个 LLM 请求在等”当成“整个 guest 可暂停”。
- [ ] 保留 daemon idle auto-pause 作为非 Agent 工作负载的 fallback。
- [ ] 手动 pause/cancel/termination 始终优先；旧完成事件不能复活新 Attempt。

**验收：** 模型等待期间 VM 驻留内存下降；响应到达后内容和控制状态正确；
手动暂停不被自动恢复覆盖；取消、断线、超时和未知完成均有明确处理；
资源配额在恢复前重新获得。

## P5：immutable rootfs/库的 DAX 与残余共享

- [ ] 解决 OverlayFS 与 `FUSE_SETUPMAPPING` 的限制，让 immutable lower
      或独立只读 rootfs 走 DAX；可写 upper 保持私有。
- [ ] 将 DAX 限定在 immutable 内容，验证写入隔离、copy-up、快照和文件 I/O。
- [ ] 评估 guest page cache 的重复页下降，而不是只看 host page cache。
- [ ] 与模板共享配合：模板负责初始内存，DAX 负责运行期只读库/工具页。
- [ ] 对不支持 DAX 的平台/文件系统明确拒绝或回退，不做静默错误共享。

**验收：** 相同库/工具在多 VM 下共享目标页；guest 私有写入不污染共享内容；
rootfs/文件系统回归通过；收益计入节点预算而不是只报单进程 PSS。

## P6：压力驱动的回收与 free-page reporting

- [ ] 用 cgroup `memory.high`、PSI 或 daemon 压力信号驱动冷页扫描/压缩，
      不再只依赖固定 250 ms 轮转。
- [ ] 评估 compressed 模式下的 free-page reporting：空闲零页应可安全回收，
      不绕过 pager 的恢复所有权。
- [ ] 在内存压力上升时优先保护前台恢复路径，限制扫描/编码 CPU 和并发。
- [ ] 明确扫描、压缩、offload 三者的优先级，避免对同一 VM 重复劳动。
- [ ] 覆盖压力下的 OOM、恢复失败、池容量拒绝和清理路径。

**验收：** 固定内存压力下 OOM/尾延迟不劣化，回收更早或更稳定；
不会因为扫描抢占导致恢复超时；free-page 回收内容正确。

## 依赖与执行顺序

1. **P0 先行**：固定本机 `pvisor-*` 启动到稳态的基线、环境和判定口径。
2. **P1 与 P2 是主线**：P1 提供暂停/配额闭环，P2 提供最大的基线共享收益；
   P1 的 pause 抽象必须先于 P2 的模板暂停，避免把 `RunOffload` 写死。
3. **P3 在 P2 之后评估边际收益**；UKSM 扫描可以并行开发，但默认启用必须等
   节点记账和模板共享稳定。
4. **P4 在 P1 的准入/恢复语义稳定后产品化**；M0 代码可复用，不重写。
5. **P5、P6 是 P2/P3 之后的残余优化**，不阻塞主线。

推荐的合并顺序：

```text
P0 本机启动到稳态 baseline
  → P1 节点内存管理 + pause 抽象
  → P2 Golden Snapshot（含 kernel sharing 验证）
  → P3 UKSM 启发式（按边际收益决定默认）
  → P4 Agent 等待产品化
  → P5 rootfs DAX
  → P6 压力驱动回收
```

## 非目标与风险

- 不默认对敌对多租户开启 KSM、物理页池或跨模板共享；内容共享必须限定信任域。
- 不把逻辑回收、encoded bytes、RSS 或单进程 PSS 当作节点物理节省。
- 不在没有匹配 cohort 的情况下更新用户 benchmark 页面或宣称密度提升。
- 不把 `RunOffload`、snapshot restore、cold pager 和模板 COW 盲目组合；
  现有互斥关系必须先验证再放宽。
- 不因页级优化牺牲恢复正确性、写入隔离、设备 I/O 和租户密钥边界。
- 不把 Agent 的等待声明当作授权；host 必须验证静止条件并保留 fallback。

## 证据与文档

- [内存子系统设计](docs/src/zh/design/memory-optimization/index.md)
- [内存去重与 adaptive scanner](docs/src/zh/design/memory-optimization/deduplication.md)
- [本地冷页压缩](docs/src/zh/design/memory-optimization/compression-local.md)
- [密封压缩池](docs/src/zh/design/memory-optimization/compression-pool.md)
- [整机 RAM offload](docs/src/zh/design/memory-optimization/offload.md)
- [共享工作集与 daemon 缺口](docs/src/zh/design/daemon/shared-working-set.md)
- [环境快照与恢复边界](docs/src/zh/design/environment-snapshot.md)
- [EXP-003 Agent I/O 与 model wait](docs/src/zh/design/experimental/agent-io-completion/index.md)
- [Benchmark 注册表与规则](benchmark/README.md)
- [内存实验协议与历史证据](benchmark.new/memory/README.md)
- [现有内存曲线与数据分析](docs/src/zh/lab-notes/vm-memory-performance-analysis.md)
