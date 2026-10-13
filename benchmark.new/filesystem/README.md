# 文件系统：一张表，一个 JSON

这里回答的问题：准备好的热缓存输入上，原生 metadata/read 完整 Python 命令需要多久？
注册条目为 [B-FS-TOOLS](../../benchmark/README.md#b-fs-tools)。当前入口只覆盖两项原生探针；
七工具内部阶段、跨后端任务及工程 A/B 的范围仍由各自注册条目约束。

[tools.json](tools.json) 是一张两行表：

```json
{
  "native-metadata": "$ python3 @REPO@/benchmark.new/filesystem/metadata_timing.py @BATCH@",
  "native-read": "$ python3 @REPO@/benchmark.new/filesystem/read_timing.py @BATCH@"
}
```

```sh
just benchmark filesystem/tools
just benchmark filesystem/tools --row native-read
just benchmark filesystem/tools native-read
just benchmark filesystem/tools --output benchmark.new/filesystem/.data/measurement
```

命令直接输出渲染表格，并生成同名 `tools.render.json`。第二个位置参数或 `--row` 只执行选中的行，
更新该行并输出合并后的表格，其他已测行保留。行名忽略大小写和多余空格，未知行会列出可选行。
当前可选行为 `native-metadata`、`native-read`；`pVisor VM` 属于 `startup/timing`，文件系统 VM 行尚未接入。
默认临时目录自动清理，只保留最终表格；显式 `--output` 保留失败诊断和成功审计证据，拒绝复用目录。

## 输入与计时

两行共用一份固定输入：32 个目录、2,048 个小文件和一个 64 MiB 数据文件。
准备时流式生成数据文件，不创建每样本副本。metadata 遍历并统计文件数和大小；
read 读取完整数据并校验 SHA256。输入大小、目录结构及所有内容在计时外核对。

每行固定 3 次预热、30 次正式样本。观察者用 `perf_counter_ns` 在创建子进程前开始，
正常零退出并收集输出后停表。解释器启动、管道收集和负载自身的校验包含在完整命令时间内；
输入准备、观察者启动和后续审计不计时。各条件顺序独立采样，报告样本标准差，不推导配对改善比例。
每次输出必须是对应操作的成功 JSON；失败或超时阻止更新最终结果。

## 代码与证据

- `prepare.py`：核对固定行命令，创建共享输入并执行逐行预检。
- `workload.py`：输入生成、完整校验与两种单次负载。
- `fs_timing.py`：共同采样、时钟和统计；`*_timing.py` 固定操作。
- `audit.py`：校验逐次命令、零退出、单调时钟、样本统计与 semspec 行映射。

保留批次含 `in.json`、`in.render.json`、`in.bench.json`、manifest 和冻结 harness，
逐次日志在 `trials/<mode>/`，正式样本在 `<mode>.samples.json`。
本地 CSV / Markdown 可通过 `update.py` 显式发布到 `.data/results/`。
旧 results 已归档到 `.data/history/`；旧 workload CLI 与 hyperfine 模板不再参与执行。

Python 时钟数据与旧 hyperfine 数据的来源不同。旧数据和撤下的公开文件保留在本地
历史归档，用户文件系统页只展示七工具对照；`docs.py publish` 会拒绝时钟范围不匹配的发布。
需要发布新口径时应登记独立文档表与 CSV，不覆盖或拼接旧样本。

## 不可变 share 的 DAX 工程 A/B

`dax_ab.py` 服务 [B-FS-ENG](../../benchmark/README.md#b-fs-eng)。这是独立的
phase-clock 配对实验，不写入 `tools.render.json`，不代表 CLI 可写 workspace。
准备命令冻结源码、优化二进制、固件、静态 C workload 和不可变输入，并提前保存
30 对平衡随机顺序。两组使用相同的只读设备，仅切换 guest 的 DAX 挂载选项。

```sh
python3 benchmark.new/filesystem/dax_ab.py --prepare benchmark.new/filesystem/.data/dax-UNIQUE \
  --rootfs PREPARED_ROOTFS --firmware PREPARED_FIRMWARE
tools/semspec/target/debug/semspec bench benchmark.new/filesystem/.data/dax-UNIQUE/in.json
```

真实测量需要 Linux x86_64 的 `/dev/kvm` 和可安装 CPU/memory 限制的 user systemd。
完整 campaign 位于同一个两核/2 GiB/零 swap cgroup，监测线程在组外的 CPU 2,3。
CPU 0,1 通过继承的 taskset affinity 限定；可用时同时设置 cpuset，缺少控制器时
记录该项为空。宿主 `RLIMIT_NOFILE` 为 65,536，避免 metadata inode 句柄耗尽。
每组 3 次预热、30 个正式新 VM；各阶段完整字节验证、零退出、DAX 属性、预算与
宿主输入不变都是成功门禁。首次访问和重复访问独立统计；重复 read/parallel 为
4 遍的每遍均值，mmap 为 16 遍的每遍均值，metadata/small 各一遍。
小文件 open/read/close 包含正常窗口回收；大文件 read 排除 OPEN，mmap 首次包含
建立映射和逐页首次访问。阶段校验和准备不计时，双线程阶段包含创建和回收线程。
失败拒绝整个批次，不删除有效慢样本。预先登记的分离分布规则、5,000 次配对
bootstrap 和所有计数见 `protocol.json`；干扰门禁是 0.5 秒的外部构建/测试扫描，
不等于排除了全部宿主噪声。原始证据和完整逐次日志保留在批次目录，
`summary.json`/`summary.csv` 与 `in.render.json` 从同一批数据生成。

## VM 缓存逐项验证

[验证报告](VM_CACHE_VALIDATION_REPORT.md)分别记录 rootfs 数据缓存、rootfs/workspace
元数据 TTL 和独立只读 no-open 的工程结果，使用 B-FS-ENG 与 B-FS-DIAG 的注册范围。
CLI 独立确认每臂 N30；快速筛选 N10、只读阶段 N5 与请求诊断分别保留。
完整七工具任务未检出加速，Cargo 有明确改善；只读 no-open 的阶段收益不外推普通
可写 workspace。之后仅将具备物理不可变声明的 rootfs 数据缓存接入默认路径，
元数据长 TTL 和 no-open 仍是原型；用户跨后端页面不使用这些数字替换既有结果。
接入条件和关闭控制见 [VM 合同](../../crates/pvisor-vm/README.md#rootfs-data-cache-default)。

完整精度数据见 [CLI 确认](vm-cache-confirm.csv)、[CLI 筛选](vm-cache-screen.csv)和
[只读阶段](vm-cache-readonly.csv)，来源及验收范围见
[收据](vm-cache-validation.provenance.json)。冻结源码、二进制、输入、运行入口和失败
尝试保留在 `.data/vm-cache-verify-20261012/`；复核使用其中的 `confirm.json`、
`screen-retry.json`、`diagnostic-retry.json`、`ro-screen.json` 和
`ro-diag.json`，均经 semspec bench 执行。复跑必须先为输出配置新的批次目录，
避免覆盖原始证据，并满足报告列出的 KVM、预算和干扰门禁。
