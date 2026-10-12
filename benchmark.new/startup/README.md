# 启动时间：一张表，一个 JSON

这里回答的问题：环境已准备好时，各执行模式到首条 Ready 输出与正常退出要等多久？
注册条目为 [B-STARTUP](../../benchmark/README.md#b-startup)。

[timing.json](timing.json) 是八行对比表，每行调用 `<backend>_timing.py`；
各后端也有独立的 `<backend>_timing.json`，输出与输入同名的 `.render.json`。

```sh
just benchmark startup/timing
just benchmark startup/timing --row Docker
just benchmark docker_timing
just benchmark pvisor_vm_timing
just benchmark native_timing
```

`--row` 只执行并渲染选中的行。默认使用自动清理的临时目录，只保存最终表格。
需要保留原始证据或发布文档时：

```sh
just benchmark startup/timing --output benchmark.new/startup/.data/measurement
python3 benchmark.new/update.py benchmark.new/startup/.data/measurement/row-0-* --startup-docs docs/src
```

每行证据保存在独立的 `row-<index>-<id>/`，显式输出目录拒绝复用。
`update.py` 的本地 CSV / Markdown 默认放在 `.data/results/`。

## 制品与输入

非原生后端通过 `--assets PATH` 或 `PVISOR_BENCHMARK_ASSETS` 指定共同输入；
FC/QEMU 通过 `--kernel-receipt PATH` 或 `PVISOR_BENCHMARK_KERNEL_RECEIPT` 指定 stock 内核收据。
代码不绑定历史批次路径或日期。Docker 默认使用输入 `assets.json` 的不可变 `docker_image`，
也可通过 `PVISOR_BENCHMARK_DOCKER_IMAGE` 指定；镜像必须预先存在。

pVisor 默认使用 `target/release/pvisor`，可用 `--binary PATH` 指定。
已有 Host listener 必须与二进制匹配。默认选当前可用的前两个 CPU；
`--cpu-affinity` 可指定准备和采样共同使用的亲和性。

## 当前运行链路

- `subjects.py`：八个后端的名称、标签和固定被测 argv。
- `common.py`：展开输入、核对制品、准备工作区并执行预检。
- `hooks.py`：重置每次输入，校验输出、canary、Run Bundle 和只读 stage。
- `timing.py`：共享事件时钟、采样和统计；各 `*_timing.py` 固定后端。
- `audit.py`：复算当前批次的配对样本并校验逐次证据。
- `publish.py`：将成功批次发布为双语 Ready / Exit 表。

文档只保存 Ready / Exit 两张结果 JSON；来源摘要共用
`docs/overrides/assets/benchmarks/startup-timing.provenance.json`，单行复测保留其他
条件的批次和配置。完整 manifest、输入清单与内核收据留在试验 `.data/`。

每行固定 3 次预热、60 次正式样本，VM 为 2 vCPU / 128 MiB，热缓存。
时钟在 Popen 前开始，收到唯一且精确的 `REFERENCE_READY` 行时记录 Ready，
正常零退出时记录 Exit。两个指标来自同次运行，使用标准库样本标准差。
解释器启动和输入准备不计时；Popen、管道观察与调度包含在内。

预检、预热与正式样本都要求正确有序输出、输入未变和零退出。
pVisor 还校验实际隔离模式与 Run Bundle；FC/QEMU 要求有序 Exit0 且无 panic。
FC 使用 stock 内核/MMIO，受控停止不能算正常退出。任何失败都阻止更新结果。
日志直接保存在试验目录；成功验证后丢弃可重建磁盘，保留纳秒记录、输出与 Bundle。

历史报告、CSV 和旧 results 已归档到 `.data/history/`，不参与当前采样、审计或发布。
工程与诊断占位表仍由 registry 登记，尚未准备的负载拒绝执行。
