# 开发工具在 pVisor、Docker 和轻量 VM 中要等多久？

## 主要结论 {#conclusions}

**在两核、热宿主缓存、每任务独立工具缓存下，七项工具到退出：pVisor host 1.07 s，staged 1.85 s，VM 5.50 s；Docker 1.41 s，Firecracker 2.74 s，QEMU microvm 2.04 s。按完整任务与单项工具分别预算等待；分离簇按各簇中位数和数量展示。**

| 需求 | 选型含义 |
| --- | --- |
| 本机执行并保留改动 | 评估 host/staged 与完整审查流程 |
| 独立 guest 内核 | 同时预算启动和 VM 工具等待 |
| 并发或闲置环境 | 需要固定资源下的吞吐与物理内存实测 |

这是 2026-10-11 使用所选冻结源码和制品、通过完整验收的跨运行时七工具对照。本批普通 rootfs/workspace 不启用 DAX，新 VM 写合并默认关闭，stage 同步默认串行。历史批次分别保留；隔离、缓存、listener 和制品条件不同，不计算跨批次优化加速比例。

<!-- measurement-status:start -->

已验收批次 `fo8e2d57ff-f`：八个后端各 30 次，七工具内部阶段与完整任务分别报告。历史数据和工程 A/B 分别保留，不合并样本。

<!-- measurement-status:end -->

## Motivation {#motivation}

Agent 的工具循环需要遍历、读写、搜索、编译和安装依赖。选型要同时看单项操作与完整等待，避免用快速启动估计文件密集任务。

## 实验设计 {#interpretation}

Linux x86_64，AMD Ryzen 7 9700X，Fedora 7.2.8-200.fc44.x86_64，Btrfs 工作区。协调器、负载与专用 rootless Docker daemon 固定到 CPU 0,1，并在同一独立 cgroup 父组内随机交替执行：总 CPU quota 为两核、内存上限 16 GiB、禁用 swap。各 VM 为 2 vCPU、16 GiB guest RAM；guest 配置容量与整组宿主上限不同。启动该父组前已计费的共享工具/镜像缓存在父组之外；进程与线程检查是可见时刻的观测，未证明所有短生命周期进程的完整 CPU 归属。host/staged 使用 rootless_process。协调器通过私有 user/mount namespace 与 owned pivot_root 隔离宿主已有 listener；namespace 准备不计时，专属 listener 在预检和预热后复用。私有 /tmp 为 2 GiB tmpfs，工作区与 stage 仍为 Btrfs；Docker bind 指向相同物理目录。用户 cgroup 未委托 cpuset，使用继承 affinity 与两核 quota，保留可见进程与线程的观测边界。

同一套离线工具与固定输入，每次新建工作区；热宿主缓存、3 次预热、每格 30 次正式采样，seed 20261011 随机交替后端。环境准备、构建、镜像导入和输入重置不计时；启动和退出计入完整任务。保留执行器提供的 TMPDIR，在其中为每任务新建私有 HOME 与 Cargo 缓存，并配置 `NODE_COMPILE_CACHE` 环境变量；Python 不读写 bytecode。七项操作的 Cargo 使用工作区内独立的 `_cargo-home`、`_tmp` 和 `_cargo-target`。临时存储随执行器配置不同，属于任务成本的一部分；目录、策略与子进程继承均校验。 逐次门禁验证缓存环境变量、父/子继承及 Python 的实际缓存行为；本批实际 Node API 启用状态和 Node 缓存目录文件系统类型均未知。10 月 10 日的独立环境检查观测到 native/host/staged/pVisor VM 的 Node 缓存使用 tmpfs，Docker 使用 overlayfs；参考 VM 输出为 ext4，但环境探测的 Ready 门禁未通过。类型与失败状态在[缓存观察 CSV](filesystem-cache-policy.csv)中分开记录，这份历史环境探测不提供本次计时或新的缓存类型证据；本批逐次校验缓存目录与父/子继承。

Docker Engine 29.7.2 使用专用 rootless overlay2 daemon、经典镜像存储和可写 bind mount；Firecracker PCI 使用冻结的独立定制参考内核、不使用 jailer，QEMU 分别使用 q35/microvm、相同参考内核与私有 ext4。pVisor VM 使用 virtio-fs 与冻结的 pVisor 固件。这里的 Firecracker/QEMU 工具数据没有测量 stock 发行版内核，不能用[独立 stock 启动结果](startup.md)替代。内核、存储和暂存语义不同，结果是这些配置下的任务成本，不是纯 VMM 或安全排名。

负载为 32 个目录中的 2,048 文件遍历、64 MiB 读取和 SHA256 校验、256 × 64 KiB 写入、git status、rg、64 个无外部依赖 Cargo 模块和 32 个离线 npm 包。单项不含启动与退出，遍历、读取等包含负载内校验；写入后的完整字节验证在写入计时之外，仍计入完整任务。完整任务包括七项、校验和退出。大型仓库、冷磁盘、在线 registry 和并发吞吐未测。

## 实验数据和分析 {#results}

测于 2026-10-11，每个后端/负载 30/30 有效，共 240 个正式任务，正式失败 0。另有能力检查与预热，均不计入统计。每个样本必须通过唯一输出、退出和执行器记录校验；staged/VM 还要求宿主工作区没有出现写入结果，upper 中全部 256 个文件的完整内容匹配。共同输入清单在采样前后校验，缓存策略在父/子负载中校验。任何错误或监测到的同用户 Rust 构建/Python 测试干扰拒绝整批；未检出这类干扰，不按耗时剔除有效慢样本。0.5 s 的进程轮询可能漏掉短暂、不可见或未识别的宿主活动。

### 启动到退出 {#complete-task}

单位 s；每个后端 N=30。通常为 P50，分离分布展示各簇中位数与数量；P95 仅作观察参考，不报告 P99。

| Runtime | 有效 / 失败 | 完整任务 P50 s | 完整任务 P95 s（参考） |
| --- | --- | --- | --- |
| Native | 30 / 0 | 1.00 | 1.02 |
| pVisor host | 30 / 0 | 1.07 | 1.15 |
| pVisor staged | 30 / 0 | 1.85 | 1.94 |
| pVisor VM | 30 / 0 | 5.50 | 6.36 |
| Docker rootless / overlay2 | 30 / 0 | 1.41 | 1.55 |
| Firecracker PCI | 30 / 0 | 2.74 | 2.80 |
| QEMU q35 | 30 / 0 | 2.06 | 2.29 |
| QEMU microvm | 30 / 0 | 2.04 | 2.38 |

| 同批完整任务对照 | 中位数差 ms | 95% CI ms | 结论 |
| --- | --- | --- | --- |
| pVisor staged − Docker rootless / overlay2 | +442.56 | [+403.53, +531.07] | 更慢 |
| pVisor VM − QEMU microvm | +3454.37 | [+3347.04, +3576.90] | 更慢 |

pVisor staged 相对 Docker rootless / overlay2的完整任务中位数差为 +442.56 ms，95% CI [+403.53, +531.07] ms；pVisor VM 相对 QEMU microvm的完整任务中位数差为 +3454.37 ms，95% CI [+3347.04, +3576.90] ms。区间来自同一批 5,000 次按轮次配对的 bootstrap（seed 20261006）；有效慢样本保留，分离分布不作单一中位数排名。存储配置与暂存能力不同，总差距不能归因于某一个组件。

### 七项工具操作 {#reference-fs}

单位 ms，每个后端/操作 N=30；P50 或分离簇的中位数与数量。操作时间排除启动与收尾；不能把七项中位数相加替代完整任务时间。

| 操作 | Native | pVisor host | pVisor staged | pVisor VM | Docker rootless / overlay2 | Firecracker PCI | QEMU q35 | QEMU microvm |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 遍历 2,048 文件 | 4.77 | 4.77 | 83.38 | 176.78 | 4.89 | 21.92 | 15.97 | 19.37 |
| 读取并校验 64 MiB | 32.55 | 33.05 | 79.96 | 138.12 | 32.67 | 80.32 | 40.43 | 46.72 |
| 写入 256 文件 | 3.72 | 3.69 (n=25); 6.44 (n=5) | 33.79 | 160.51 | 3.79 | 43.02 | 5.35 (n=25); 26.02 (n=5) | 5.53 (n=23); 14.60 (n=7) |
| git status | 15.95 | 16.14 | 171.21 | 572.05 | 17.08 | 131.64 | 86.04 | 163.52 |
| Ripgrep 搜索 | 8.09 | 8.70 | 127.39 | 485.08 | 9.15 | 16.23 | 14.33 | 14.22 |
| 离线 Cargo 编译 | 52.50 | 57.36 | 89.97 | 618.73 | 50.05 (n=18); 144.54 (n=12) | 406.59 | 347.73 | 363.63 |
| 离线 npm 安装 | 215.85 | 226.38 | 249.46 | 1160.44 | 223.97 | 573.43 | 449.03 | 474.68 |


文件遍历、Git 与搜索需要经过暂存文件视图；读取、编译和离线安装的成本还包含校验、工具运行与各执行器的临时存储行为。按实际工具组合和完整任务等待设置超时，不能仅凭启动速度估计总成本。

<a id="full-ubuntu"></a>
完整 Ubuntu 文件负载尚未在这组当前构建与资源条件下测量。

### 文件系统优化的适用范围 {#cache-status}

表中冻结构建包含宿主 copy-up 调度、读队列、journal 锁、空闲调度与后续 VM 队列/I/O 屏障修复；独立工程 A/B 见[完整证据范围](../design/filesystem-performance-analysis.md#filesystem-status)。跨运行时结果衡量当前默认配置的任务成本，局部工程 A/B 的百分比不用于折算上述数据。

本批已用当前冻结制品重新测量全部八个后端，并完成正确性、输入、预算和干扰门禁。此前未完成验收的尝试继续单独保留，不进入本页统计；这份新跨运行时结果本身不证明每项修改的因果收益。

普通 rootfs/workspace 默认关闭 DAX，不创建 DAX 窗口；不可变 share 保留显式启用入口，见[DAX 对照](../design/filesystem-performance-analysis.md#readonly-dax-ab)。新建 VM 的写合并默认关闭，可用 `PVISOR_VM_FS_WRITEBACK=1` / `0` 显式配置；默认恢复已协商快照时保留原模式，显式 `PVISOR_VM_FS_WRITEBACK=0` 会拒绝已开启写合并的快照。独立[写合并对照](../design/filesystem-performance-analysis.md#writeback-ab)使用其自身冻结制品，不能用于折算本批任务时间。

不可变 lower 的物理元数据缓存要求调用者显式声明并保持 lower 稳定；宿主 rootfs、OCI 解包目录与 lazy 镜像本地投影不自动获得该承诺。扩展 Linux HOST 内核缓存尚未接入默认 `pvisor run` 或 VM，其正式计时没有通过验收。能力边界见[不可变 lower 缓存分析](../design/filesystem-performance-analysis.md#immutable-lower-cache)与[内核缓存分析](../design/filesystem-performance-analysis.md#host-kernel-cache)。

`PVISOR_STAGE_SYNC_WORKERS` 的并行 stage 同步选项未开启，仍没有独立的串行/并行性能对照；不能把该实现的存在解释为上表的加速来源。

### 历史 Hyperfine：原生单操作的完整命令时间 {#hyperfine-native}

已发布的独立 hyperfine 批次使用 32 个目录中的 2,048 个文件和 64 MiB 读取输入，每项 3 次预热、30 次正式样本，保留热缓存。
metadata 校验文件数、目录数和总字节；read 校验完整内容的 SHA256。hyperfine 直接执行 Python 负载；解释器启动和校验属于完整命令时间，准备不计时。
这些数值与上方七工具任务的内部阶段、跨后端排名及整体任务预算不同，不能替换它们。

|  | N | Median ms | Mean ms | SD ms | Min ms | Max ms |
| --- | --- | --- | --- | --- | --- | --- |
| native-metadata | 30 | 23.94 | 23.99 | 0.40 | 23.34 | 25.10 |
| native-read | 30 | 57.81 | 57.84 | 0.88 | 55.68 | 59.31 |

[完整精度 CSV](filesystem-semspec.csv) · [配置与制品来源](../assets/benchmarks/filesystem-semspec.provenance.json)。
当前 `benchmark.new/filesystem/tools.json` 使用独立 Python 时钟，见[复现与计时说明](https://github.com/DeepLink-org/pvisor/blob/main/benchmark.new/filesystem/README.md)。其计时口径已有变化，不能通过 `docs.py publish` 写入这张历史 hyperfine 表；需要独立文档绑定和 CSV。

### 数据下载与复现 {#run}

[已验收文件系统统计 CSV](filesystem.csv) · [同批差异与 95% 置信区间](filesystem-comparisons.csv) · [测量制品、源码与输入来源](filesystem-provenance.csv) · [验收记录](../assets/benchmarks/filesystem-publication-audit.json) · [比较方法](methodology.md) · [复现手册](https://github.com/DeepLink-org/pvisor/blob/main/benchmark/pvisor/README.md#same-host-runtime-comparison)与[整组资源预算](https://github.com/DeepLink-org/pvisor/blob/main/benchmark/pvisor/README.md#explicit-whole-parent-resource-budget)

本批原始证据为本地 `benchmark/.data/fo8e2d57ff-f/report.json`，冻结源码、构建收据与执行/验收记录在 `benchmark.new/filesystem/.data/fs-opt-report-20261011/`；原始样本与制品不作为站点下载。此前有效批次和失败尝试分别保留，见[来源清单](evidence-sources.csv)；历史[运行时统计](runtime-summary.csv)和[来源](runtime-provenance.csv)不与本批合并。
