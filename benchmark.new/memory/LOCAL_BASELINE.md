# 本机 pVisor VM 启动至稳态基线（2026-10-22）

这是 `B-MEMORY-SCALE` 的工程预检记录。full 每条件 N=1，quick 在独立目录重复六个条件；
两批不合并，不提供置信区间、生产密度或外部产品排名。
以下 MiB 均为四 VM 完整产品组的 cgroup 计费内存，包含池服务，不是单 VM 占用，
也不等于整机新增物理占用。原始 PSS 与计费曲线分别保留。
链接到 `target/tmp/` 的制品仅在本机保留，不随 Git 发布。

运行记录使用真实目录，命名为 `full-日期-序号` 或 `quick-日期-序号`（UTC 日期，各模式当日序号独立递增）：

- [full-20261022-1：full](../../target/tmp/full-20261022-1/)
- [quick-20261022-1：quick](../../target/tmp/quick-20261022-1/)

配置和版本放在记录内的 manifest，成功状态看 `baseline.json`。
记录已从旧目录真实迁移，未使用符号链接；原始测量文件未改写，记录中的执行命令保留历史路径。KSM 已恢复为原 `run=0`。

## 图中各维度与 full / quick 的关系

- 行表示内容类型：`repeated` 易压缩且重复，`random-shared` 跨 VM 相同但难压缩，
  `random-unique` 各 VM 独有且难压缩。
- 列表示同一次实验的两种观察：整组计费内存，以及按共享比例分摊的进程 PSS。
  计费内存用于主要占用判断；PSS 辅助检查共享和驻留，不含全部内核及未映射成本。
- 每条线表示内存机制：off、local、physical、KSM 或 compressed。
  full 包含五模式 × 三种内容，共 15 个条件，绘成三行 × 两列。
- quick 是独立复测配置：只测 off/physical/KSM × repeated/random-shared，
  共六个条件，绘成两行 × 两列、每格三条线；没有名为 `pvisor-quick` 的模式。
  下文 quick 一节提供其独立曲线，不与 full 混合。

稳态时间、稳态占用及波动直接回答优化目标。回收曲线说明启动后占用如何下降，
但达到“减少 128 MiB”可能仍继续变化，也可能遗漏 Ready 前已发生的共享，故仅作诊断。
CPU 是代价指标：较早到达低占用若需要大量后台扫描，仍需结合运行预算判断是否值得。
启动峰值、读回和写后回升同样保留为代价指标，不取代主要目标，也不单凭 CPU 排名。

## 环境、输入与判据

- 本机：AMD EPYC 9654，Linux x86_64/KVM，内核 `6.8.0-136-generic`。
- 每组四个 256 MiB / 1 vCPU VM，各持有 64 MiB 测试数据；整组四核配额、
  CPU 0–3、2 GiB、零 swap、256 任务上限。每个条件创建新 VM 及所需池。
- governor 为 `schedutil`，boost 为 `1`；两批 KSM 均为 `run=1`、
  `pages_to_scan=100`、`sleep_millisecs=20`、`smart_scan=1`。
  非 KSM 模式不接受 KSM advice；全局 ksmd CPU 在组外单列。
- 复用已冻结且与当前产品源码匹配的
  [pvisor-only 输入](../../target/tmp/pvisor-only-20261021/inputs/receipt.json)，测量期间未构建。
  full / quick 的产品输入、源码 manifest、harness、稳态规则及受检宿主配置一致。
- 全部 VM Ready 后固定观察 60 秒，再做完整读回、25/100% 私有写入和 30 秒写后观察。
  计费内存每 50 ms、PSS 每 500 ms 采样；每个条件前保留安静等待。
- `pvisor-observed-steady/v1`：寻找持续至窗口结束且至少 10 秒的最早后缀，
  其计费内存极差不超过 `max(4 MiB, 后缀中位数的 2%)`。
  启动从首份组内快照计时，Ready 指四 VM 全部就绪；“未达”表示窗口内不满足判据，
  不以 60 秒读数或持续回收 128 MiB 的阈值代替稳态。

## full：早期稳定与较低占用的取舍

下表稳态占用为判定后缀的中位数，每格 N=1，单位 MiB。

| 模式 | 重复、易压缩内容 | 跨 VM 相同随机内容 | 各 VM 独有随机内容 |
|---|---:|---:|---:|
| pvisor-off | 405.49 | 405.44 | 405.46 |
| pvisor-physical | 156.54 | 229.96 | 395.76 |
| pvisor-ksm | 117.91 | 180.18 | 372.54 |
| pvisor-local | 未达（60 秒 121.37） | 未达（60 秒 378.01） | 未达（60 秒 378.04） |
| pvisor-compressed | 未达（60 秒 126.86） | 未达（60 秒 381.55） | 未达（60 秒 382.23） |

| 内容 | 模式 | 启动至稳态 s | Ready 至稳态 s | 稳态极差 MiB |
|---|---|---:|---:|---:|
| 重复 | pvisor-off | 1.74 | 0.00 | 1.17 |
| 重复 | pvisor-physical | 4.35 | 2.51 | 1.25 |
| 重复 | pvisor-ksm | 35.85 | 34.11 | 3.30 |
| 相同随机 | pvisor-off | 1.81 | 0.00 | 1.23 |
| 相同随机 | pvisor-physical | 4.86 | 2.92 | 3.95 |
| 相同随机 | pvisor-ksm | 35.81 | 33.96 | 3.87 |

物理池在重复/相同随机内容上较早进入低占用平台，KSM 等待更久但平台更低。
off 在 Ready 时已经稳定，但保持约 405 MiB 的较高占用。
两条压缩路径在约 30 秒附近回升、窗口末尾再次下降，六个空闲条件均未满足稳态判据；
这不是采样缺失，不能把较低的 60 秒值改称稳态。回升的触发原因尚未定位。

![full：启动至稳态，计费内存与 PSS 分列](../../target/tmp/full-20261022-1/analysis/startup-to-steady.svg)

完整的 [Ready 后空闲曲线](../../target/tmp/full-20261022-1/analysis/idle.svg)和
[逐条件摘要](../../target/tmp/full-20261022-1/analysis/summary.json)包含三类内容、全部模式、
2/5/10/30/60 秒读数、内存时间积分及峰值。

## CPU、读回与写后占用

相同随机内容的 60 秒空闲组 CPU 为：off 2.03 CPU 秒、物理池 18.69 CPU 秒、
KSM 2.09 CPU 秒；KSM 另有 1.71 CPU 秒的全局 ksmd 成本，位于组配额之外，
不能完全归因到本组。组内 CPU 包含采样、控制和 VM 成本，不是纯扫描器 CPU。
独有随机内容中，物理池组 CPU 为 34.79 CPU 秒，稳态仍为 395.76 MiB，
说明无重复业务页的条件必须与有利内容一起保留。

重复内容完整读回的计费内存如下，每格 N=1，单位 MiB：

| 模式 | 读回前（60 秒） | 读回后 |
|---|---:|---:|
| pvisor-off | 405.96 | 406.61 |
| pvisor-physical | 156.45 | 156.94 |
| pvisor-local | 121.37 | 378.69 |
| pvisor-compressed | 126.86 | 384.18 |
| pvisor-ksm | 117.87 | 118.75 |

共享路径在读回时保留低占用；压缩路径恢复私有 RAM，空闲低位不保留。
相同随机内容在 100% 私有写入后，物理池的写后稳态为 437.07 MiB，
高于 off 的 406.93 MiB；该条件整个生命周期峰值为 487.27 MiB。
因此不能用初始空闲稳态节省直接推算可容纳 VM 数量。

![full：读回与私有写入后的占用](../../target/tmp/full-20261022-1/analysis/readback-and-write.svg)

## quick：独立复测

以下仅并列六个重叠条件的两次观测，不合并样本。占用为稳态后缀中位数。

| 内容 | 模式 | full Ready→稳态 s | quick Ready→稳态 s | full MiB | quick MiB |
|---|---|---:|---:|---:|---:|
| 重复 | pvisor-off | 0.00 | 0.00 | 405.49 | 405.42 |
| 重复 | pvisor-physical | 2.51 | 2.54 | 156.54 | 157.13 |
| 重复 | pvisor-ksm | 34.11 | 34.13 | 117.91 | 117.67 |
| 相同随机 | pvisor-off | 0.00 | 0.00 | 405.44 | 405.46 |
| 相同随机 | pvisor-physical | 2.92 | 2.69 | 229.96 | 229.64 |
| 相同随机 | pvisor-ksm | 33.96 | 34.20 | 180.18 | 181.06 |

这六个条件两轮都达到稳态；占用差最大为 0.875 MiB，Ready 至稳态时间差最大约 0.240 秒。
两轮均观察到物理池较早稳定、KSM 平台较低。压缩模式与独有内容没有 quick 重复证据。
原始批次仍分开保留，差值与来源摘要见
[重复性记录](../../target/tmp/full-20261022-1/repeatability.json)。

![quick：启动至稳态复测](../../target/tmp/quick-20261022-1/analysis/startup-to-steady.svg)

## 验证与后续使用

- full 15/15、quick 6/6 条件通过；分别完成 870 / 348 项内容、私有写入、peer、退出检查。
  两批均无 OOM/swap/配额失败，专属资源组均完成清理。
- 两份 `baseline.json` 均为 `complete=true`、`evidence_valid=true`，来源、观察摘要、
  启动/空闲/写后覆盖及环境守护均通过。全部 21 个条件证据充分；full 的六个压缩空闲
  条件为有效的“未达稳态”，不是失败样本。
- [full manifest](../../target/tmp/full-20261022-1/manifest.json)、
  [full 状态](../../target/tmp/full-20261022-1/baseline.json)、
  [quick manifest](../../target/tmp/quick-20261022-1/manifest.json)、
  [quick 状态](../../target/tmp/quick-20261022-1/baseline.json)绑定产品输入、源码归档、harness 和判据。
- 首次 `p0-1022f1` 在 VM 启动前因协调器函数命名冲突停止，零条件完成；
  修复并增加离线回归测试后，在新目录重新冻结了完整 full。失败目录已按要求清理，不混入结果。
- 后续优化以本报告对应制品为旧 baseline，保持相同负载、资源和判据，安排新旧制品同批
  对照；优先观察物理池的稳态平台/CPU 和压缩曲线回升，同时继续保留启动、读回及写后成本。
  本报告不据此改变默认机制或放宽准入。
