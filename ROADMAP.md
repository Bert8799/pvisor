# pVisor 启动延迟优化路线图

> 本文件是 `feature/startup_opt` 工作分支的临时路线图，不属于在线文档。
> 合并到 `main` 后删除本文件；如果需要长期保留，应另行决定归档位置。

## 目标

降低 pVisor 的 VM Ready 中位数，以原有 release 为基线，逐项验证改动是否带来可重复的提升。
保留隔离、执行记录、恢复兼容性和资源清理语义。
以内存效率和高密度部署为实现约束，优先消除重复计算、读取、复制和临时分配。

推进顺序：**固定测试 → 缩短新建 VM 关键路径 → 工作集预取 → 模板快速实例化 → 预热池 → 动态预热**。
先完成容易独立验证的改动，再推进需要新增生命周期管理的能力。

## 已完成的工作

下列勾选表示已有代码实现

- [x] 合并非 Gateway 路径的重复初始 RunRecord 写入。
- [x] 未变化的 Run 索引复用内容和 inode，保留必要同步与失效修复。
- [x] runner spec 和临时 attestation 使用完整写入的私有 IPC，移除不必要的磁盘同步。
- [x] 新建目录树的同步屏障去重。
- [x] ARM64 为每台新 VM 注入独立的宿主随机启动熵；失败阻止启动。
- [x] 静态 Rust guest supervisor、按设备需求裁剪固件、内置内核 compact 打包。
- [x] 可执行文件已验证摘要复用、事件唤醒和已完成进程树清理去重。
- [x] 镜像文件按需读取，VM direct backend，元数据及内容本地缓存。
- [x] 有界目录元数据预取、客户端二进制索引页及已解码节点缓存。
- [x] 上游连接池和私有 Unix 网络桥连接复用。
- [x] 执行快照捕获/恢复、RAM 按需校验/解码、恢复写入私有 COW。
- [x] 同一快照恢复的挂载 owner 复用及 node backing 的 pin/释放基础。
- [x] Daemon 原生 VM 创建中的独立 generation、token、状态目录、cgroup 与失败清理。
- [x] Ready 计时函数、Run Bundle 校验、常态阶段日志、可选启动细分 profile。

实现与证据入口：

- [启动技术分析与历史实验](docs/src/zh/lab-notes/vm-startup-performance-analysis.md)
- [持久化实现](crates/pvisor-journal/src/persistence.rs)
- [镜像读取器 V2 实验](benchmark/pvisor/LAZY_IMAGE_V2_REPORT.md)
- [客户端索引页实验](benchmark/pvisor/LAZY_INDEX_PAGES_REPORT.md)
- [私有桥连接复用实验](benchmark/pvisor/LAZY_BRIDGE_REPORT.md)
- [快照设计与历史证据边界](docs/src/zh/design/environment-snapshot.md)
- [共享工作集、原生基础与 Daemon 缺口](docs/src/zh/design/daemon/shared-working-set.md)

## P0：尽快固定测试脚本和方法

直接使用[现有启动 benchmark](benchmark.new/startup/README.md) 的 pVisor VM 单后端入口，
固定以下两项：

- [x] 正式采样固定为 **60 次**；保留原有 **3 次预热**。
- [x] 运行时设置 **`PVISOR_KRUN_ENOMEM_WORKAROUND=0`**，关闭每次 `KVM_RUN`
      前额外休眠 5 ms 的兼容 workaround。

其余命令、输入准备、计时、校验、统计和服务生命周期全部沿用原 benchmark。
不另写测试框架，不增加额外预热、服务重启或隔离流程。

每次只测一个指定的 release：先测原版本并保存基线，优化后用同一 benchmark
单独测新版本，再比较 VM Ready 中位数。统一入口为
`benchmark.new/startup/vm_ready.py --binary PATH`。

**验收：** 一次运行得到该版本的 60 个有效 VM Ready 样本及中位数；
后续优化使用相同方法相对基线验证提升。

## P1：缩短新建 VM 的启动关键路径

- [x] **将内置内核的身份查询与解包分开。** Linux x86_64 musl 构建在
      `build_kernel.rs` 中对实际未压缩内核字节（含填充）计算 SHA-256，写入生成代码的常量。
      `VmPlatform::embedded_kernel_sha256()` 直接返回该常量；固件解析、runner 的内核存在性
      检查及 checkpoint 兼容性绑定改用此接口，省去这些查询触发的内核解包、分配和运行时哈希。
      真正装载内核时仍通过原有惰性解包与私有映射复制流程取得内核内容；动态固件路径继续读取
      并校验所选固件文件。实现见 [内核构建](crates/pvisor-vm/build_kernel.rs)、
      [固件接口](crates/pvisor-vm/src/firmware.rs) 与 [VM 启动](crates/pvisor/src/executor/vm/supported.rs)。

- [x] **合并实例控制记录与 checkpoint 所需的可执行文件读取。** 启用实例控制时，
      Linux VM 若声明支持 checkpoint 且未启用运行时 VM 网络强制执行，发布控制记录前从
      `/proc/self/exe` 打开的同一描述符流式读取文件，用 64 KiB 缓冲区分别计算 BLAKE3 和
      SHA-256。BLAKE3 写入实例控制记录；后续 checkpoint 兼容性绑定重新打开可执行文件路径，
      经身份核对后复用已计算的 SHA-256，省去第二次完整读取与哈希。
      进程内缓存仅保留一份文件身份、摘要和固定 inode 的描述符，不保留文件内容；仅允许缓存
      与 Linux 正在执行、受内核写保护的 inode 匹配的文件，并核对设备号、inode、大小、mtime、
      ctime、权限和 UID/GID。读取前后身份变化则报错；身份不匹配、摘要尚未计算或路径被替换时
      重新读取计算。普通进程和启用 VM 网络强制执行的路径仅为控制记录计算 BLAKE3，macOS
      不复用此缓存；前端自校验保持独立。取舍：SHA-256 提前到控制记录准备阶段，某些最终不允许
      捕获的特殊 VM 配置可能计算一次用不到的摘要。实现见
      [运行时摘要缓存](crates/pvisor/src/runtime/executable.rs)、
      [实例控制记录](crates/pvisor/src/runtime/instance_control.rs) 与
      [兼容性绑定](crates/pvisor/src/executor/vm/checkpoint.rs)。

- [x] **将默认快照存储的子目录初始化移到捕获请求阶段。** 未显式配置文件系统池时，
      VM 启动调用 `SnapshotStore::prepare_launch()`：持久化创建并固定私有存储根目录，
      校验已有子目录及 `captures` 的类型，同时完成兼容性和 Run/Attempt 身份绑定；缺失的
      存储子目录不在启动阶段创建。收到 checkpoint 或 suspend 捕获请求后，控制端先在
      `spawn_blocking` 中调用 `LaunchBinding::capture_directory()`，完整初始化存储子目录，
      持久化创建 `captures` 并分配本次捕获的临时目录，全部成功后才发送 guest 冻结请求。
      显式文件系统池仍在启动前完整初始化与校验，公共 `SnapshotStore::new()` 保持原有行为。
      取舍：首次捕获承担缺失目录的创建与同步成本，相关错误延后到捕获请求返回；目录准备失败
      不发送冻结请求、不产生新快照，guest 继续运行。需要保存可恢复状态时，必须确认捕获成功
      后再退出 VM，旧快照不包含上次捕获后的改动。实现见
      [存储初始化](crates/pvisor/src/environment_snapshot/store.rs)、
      [捕获目录准备](crates/pvisor/src/executor/vm/checkpoint.rs) 与
      [控制请求顺序](crates/pvisor/src/executor/vm/control.rs)。

未通过验收并已回退的方案：`sha2` 速度优先编译、内核直接解包到私有中间映射或最终
RAM backing、Linux uevent 唤醒 guest stdio 回查，以及并行计算可执行文件摘要与准备
快照存储。这些尝试未获得可重复的 VM Ready 改善。

ENOMEM workaround 的默认策略作为独立兼容性工作验证，不纳入 P1。
P0 测量时显式关闭该开关。

**验收：** 使用 P0 的固定流程对照改动前已保留的 release，确认 Ready 改善；
相关包的 `just test` 和受影响消费者编译通过。没有收益的改动不作为性能优化保留。

## P2：延续懒加载，加入启动工作集 Prefetch

复用已有按需镜像读取、元数据预取、内容缓存和连接复用，进一步减少启动时的阻塞读取。

- [ ] 固定 Python/Node/Agent 启动负载，采集文件及读取范围，区分关键访问与后续访问。
- [ ] 先用静态工作集清单验证预取开关的收益，再实现自动采集与策略。
- [ ] 将工作集绑定镜像和 Runtime/Agent 版本，定义失效与更新机制。
- [ ] 明确预取落入哪一层缓存，设置字节、并发和时间预算，优先保障前台请求。
- [ ] 同时检查首个有效结果、任务完成时间和预取成本，避免只把等待移到 Ready 之后。

**验收：** 固定负载相对无预取基线启动更快，额外读取和资源消耗可控。
该负载的测试方法确定后同样固定；shell VM Ready 与 Agent 就绪分别统计。

## P3：将模板快速实例化接入 Daemon

复用已有快照恢复、RAM 按需读取、私有 COW 和实例生命周期管理，减少重复引导工作。

- [ ] 定义可等待新任务的通用启动模板，以及生成、兼容性、版本和回收规则。
- [ ] 在 Daemon create 流程中接入模板恢复，交付新命令、环境和 workspace。
- [ ] 为每个实例分配独立身份、凭据、控制连接和可写状态，处理随机状态与网络初始化。
- [ ] 覆盖并发创建、取消、恢复失败、资源释放和 Daemon 重启后的状态核对。

**验收：** 新任务的请求至就绪时间相对普通创建下降，实例之间保持独立。
固定请求至就绪的测试流程，模板制作成本另记，不与新建 VM 的结果混合。

## P4：固定容量 VM Prewarming Pool

在创建与清理流程稳定后，维护少量已就绪实例，缩短请求等待。

- [ ] 先实现固定容量、独占领取、使用后销毁的池，复用 Daemon 的生命周期管理。
- [ ] 定义 warming、ready、leased、failed、draining 状态与真实就绪检查。
- [ ] 完成后台补充、过期回收、池耗尽回退和失败清理。
- [ ] 将创建中的实例计入容量，约束预热 CPU、内存和 I/O 占用。
- [ ] 固定池命中、池耗尽、突发请求和持续请求的测试负载。

**验收：** 相对直接创建降低请求等待，同时报告命中率、补充速度与空闲资源成本。

## P5：动态预热容量

在固定池运行稳定后，根据请求需求和资源压力调整预热数量。

- [ ] 根据请求速率、补充时间和池耗尽情况估计所需容量。
- [ ] 结合 CPU、内存、I/O 压力和前台延迟调整容量。
- [ ] 设置上下限、冷却时间与滞回，压力上升时优先收缩预热资源。
- [ ] 使用固定负载对照固定容量策略，覆盖突发、持续高负载和空闲阶段。

**验收：** 在相同资源预算下减少等待，或在相近等待水平下减少资源占用。

## 执行节奏

1. 先完成 P0，固定脚本、基线和测试方法，再开始产品改动。
2. 从 P1 选择一项：检查代码 → 实现最小改动 → 正确性验证 → 使用同一 benchmark 单独测量 → 对照基线决定保留或回退。
3. 记录通过验收的改动；每次对照保留明确的原有 release 基线。
4. P1 稳定推进后开展 P2；P3、P4、P5 按前置能力依次实施。

本路线图仅维护计划和验收标准；数据与运行记录保存在 `target/tmp/`。
