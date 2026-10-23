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

已完成完整 full 和独立 quick 复测，详见[数据与曲线](target/tmp/full-20261022-1/README.md)：

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

## P1：缩短启动至稳态时间，降低稳态占用

以 P0 的四 VM 静态基线为起点，优先优化运行中 VM 的初始共享和扫描路径。
当前相同随机内容下，物理池约 4.86 秒进入 229.96 MiB 稳态，KSM 约 35.81 秒进入
180.18 MiB 稳态。先定位物理池仍未共享的驻留成本与早期扫描延迟，避免只增加扫描
频率或只降低窗口末读数。暂停/offload 不属于本阶段的收益来源。

### 实现与对照

- [x] 分解 off/physical/ksm 的启动、空闲稳态与读回成本，结合计费分类、PSS、
      池存储和扫描统计提出具体假设；区分尚未扫描、无重复页和共享管理成本。
- [ ] 每次只改一个变量，优先检查物理池扫描/候选生命周期、重复校验和不可变初始页共享；
      保持共享授权、完整字节比较、COW 写入隔离及设备内存访问保护。
- [ ] 先运行无需 VM 的针对性回归；确需测量时，使用相同环境和协议的新旧制品受控对照。
      KSM 由管理员预置，脚本只检查；保留每个候选及失败结果，不覆盖 P0 输入或原始数据。
- [ ] quick 用于迭代筛选；候选通过后补齐受影响的 full 条件，尤其 random-unique
      和压缩负载，记录 CPU、启动峰值与访问后回升，避免以一种有利内容决定默认策略。

已完成第一轮候选筛选，详见[实验记录](target/tmp/quick-20261022-3/README.md)：

- 物理池描述符权限校验缓存保持默认关闭；精确 Arc 身份固定、替换描述符拒绝及 COW
  回归通过。VM 测试 550 项通过、8 项跳过，Python 内存实验 68 项通过，Rust/Python lint 通过。
- 使用同一冻结制品，关闭/开启各运行一轮 quick（`quick-20261022-2` / `quick-20261022-3`），
  两轮各 6/6 条件、348 项检查通过，输入、源码清单、harness 和受检宿主配置一致。
- random-shared 启动至稳态为 4.833 → 4.742 秒，占用 230.29 → 230.67 MiB；
  repeated 为 4.333 → 4.323 秒、156.43 → 157.82 MiB。空闲 CPU 均未降低。
  时间差小于控制条件的最大变化，尚不支持收益结论，不扩跑 full、不默认启用。
- 下一候选优先定位约 7.3 万个 pool VMA、约 19.5 MiB slab 与残余匿名驻留的来源，
  再确定映射聚合或初始页共享方案；不能仅靠提高扫描频率。

全零页候选已完成一轮小范围 A/B，详见[对应运行记录](target/tmp/quick-20261023-2/README.md)：

- 两次屏障完整复核后，以惰性匿名零页替换全零物理池页，减少零页池发布和映射；
  默认关闭。离线测试确认 32 个逐页 VMA 可合并到不超过 2 个，私有写入和竞争拒绝通过。
- VM 回归 552 项通过、8 项跳过，Python 内存实验 69 项通过，Rust/Python lint 通过。
  同制品关闭/开启各运行物理池两内容子集，两批各 2/2 条件、116 项检查通过，
  `profile_subset=true`，不冒充完整 quick。
- random-shared 稳态占用 229.85 → 229.23 MiB，启动至稳态 4.824 → 4.518 秒；
  repeated 为 156.83 → 155.89 MiB、4.207 → 4.213 秒。pool VMA 各减少约 2,600，
  slab 约减少 0.5 MiB，VM PSS 变化很小；存在小幅正向趋势。
- 读回略慢，random-shared 的稳态后缀波动变大；N=1 且没有交替重复和完整矩阵证据。
  保持默认关闭，不以这点变化作为后续主方向；P1 仍未完成验收。

初始内核映像共享已有可重复的占用收益，详见[固件布局对照记录](target/tmp/quick-20261023-4/README.md)：

- 使用同一 vmlinux 生成 compact/flat 固件，完整映像、guest 地址及 ABI 一致；
  GNU 动态加载路径利用现有 flat 格式的文件私有映射与 COW，减少每 VM 匿名重建。
  probe、daemon 和 guest worker 二进制相同，不改 pager 授权或排除范围。
- 两种布局均在计量组内创建新固件 inode，将复制 CPU、文件缓存和较大文件的成本计入。
  这是新的配对计费边界，不能直接拿 P0 外部预热固件的读数计算优化比例。
- random-shared 的 off/physical 各完成一轮对照，再反序重复 physical，合计 6 个条件、
  348 项检查通过。physical 两次稳态为 236.53/237.06 → 220.48/220.29 MiB，
  分别下降 16.04/16.77 MiB（约 6.8–7.1%），读回及私有写入后差异仍保留。
- 启动至稳态分别为 4.485/4.759 → 4.830/4.814 秒，多 0.345/0.055 秒；
  尚无更快稳定的证据。CPU 接近，首次读回差异未重复；固件文件增加约 5.25 MiB。
- **取得了降低稳态占用的候选进展，尚未完成 P1 验收。** 默认 compact 不变。
  下一步拆分 Ready 后扫描/映射发布/旧页释放的时间，定位约 4.8 秒的稳态耗时；
  候选通过后再补齐受影响的 full 条件。musl 嵌入内核路径仍复制映像，需单独实现和验证，
  不能直接外推本次动态加载结果。

### 验收

- [ ] 在相同模式的新旧实现之间比较启动至稳态时间、稳态中位数及波动；
      至少一项改善，另一项无实质回退，且完整性/写入隔离/退出检查通过。
      跨模式差异用于选择方向，不能当作代码改动的改善比例。
- [ ] CPU、峰值或读回成本增加时明确报告取舍；没有受控数据支持的候选不默认启用。
- [ ] 仍未达稳态的条件如实保留；阶段结果写入本机工程记录，不宣称密度或生产收益。

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

## 后续：节点内存管理与暂停回收

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

当前实现进度：

- [x] 接入按实例持久化的暂停策略：默认 `vcpu`，显式选择 `offload` 时复用现有
      file-backed RAM 回收和同 Attempt 恢复；共享池不兼容时明确拒绝。
- [x] 在原生 pause/resume 路径记录同一实例/Attempt 的计费内存前后值、内存分类、
      swap、触顶/OOM 计数和控制耗时；失败确认保留未完成记录，观测差值不转为准入额度。
- [ ] 验证真实暂停回收量与恢复语义，再实现配额释放和恢复前准入。
      当前两种策略均保留完整准入额度，策略接入尚未完成暂停回收闭环。

已通过 `just fmt-rust`、`just lint-rust -p pvisor-daemon` 和
`just test pvisor-daemon`（112 项通过、2 项跳过，包含新增的内存观测离线测试）。
未启动真实 VM，尚无暂停回收收益数据。
真实验收还需要可用的 KVM/FUSE 与完成服务初始化的 prepared image，
确认内存下降、恢复后的内容/文件 I/O 和服务就绪后，再推进配额释放。

**验收：** 暂停实例确实降低 `memory.current`；resume 不产生 OOM；
暂停/恢复内容、文件 I/O 和状态正确；配额释放比例可配置且有上限；
`RunOffload` 之外的恢复路径有明确 capability 拒绝。


## 依赖与执行顺序

1. **P0 固定口径**：本机 baseline 已建立，继续保留原始数据与冻结输入。
2. **P1 为当前主线**：先完成一个直接改善启动至稳态时间/占用的受控优化。
3. **P2/P3 按证据推进**：模板与不可变页共享、扫描策略是后续候选；
   P1 可先落实其中有清晰依据的小改动，不以暂停准入闭环作为前置条件。
4. **P5/P6** 处理共享与压缩的残余成本；按 P0 口径验证实际收益。
5. **暂停/节点准入及 P4** 另行推进，用于等待阶段的内存回收和有效容量，
   不代替启动稳态优化验收。已有 opt-in 策略和观测保留，暂不继续扩展。

当前顺序：`P0 基线 → P1 单变量优化与受控验证 → 按证据选择模板/扫描/不可变页共享`。

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
