# 发布资源预算

资源预算配置在 `private/repositories.yaml` 的顶层 `resource_limits` 中，接收器与 Publisher 使用同一份文件。缺省部署也会启用预算；不需要重新运行初始化。模板见 [repositories.example.yaml](../config/repositories.example.yaml)。这里只限制发布端，不限制 Gitea 接收 Push 的仓库体积，也不代替容器、磁盘或文件系统配额。

## 配置与生效

以下是全部默认值，单位为字节、秒或记录数，必须填写正整数。未填写的字段使用默认值，未知字段和布尔值会被拒绝：

```yaml
resource_limits:
  git_repository_bytes: 2147483648
  git_total_bytes: 8589934592
  git_fetch_bytes: 536870912
  git_min_free_bytes: 536870912
  git_timeout_seconds: 120
  git_process_memory_bytes: 536870912
  git_output_bytes: 8388608
  scan_entries: 200000
  queue_active_records: 1000
  queue_total_records: 10000
  queue_reserved_bytes: 1073741824
```

这些是整个部署的预算，不是每个仓库分别拥有的队列额度。`queue_active_records` 不能大于 `queue_total_records`。仓库原有的 `max_files`、`max_bytes`、`max_file_bytes` 继续限制单次导出的内容，不能用它们替代 Git 历史容量预算。

已有服务变更配置时，先备份私有文件，在维护窗口中编辑并校验 YAML，再重建两个使用单文件挂载的容器：

```sh
cp -p private/repositories.yaml private/repositories.yaml.before-resource-change
# 编辑 private/repositories.yaml，保留仓库、endpoint 和其他现有配置。
python3 -c 'import yaml; yaml.safe_load(open("private/repositories.yaml"))'
docker compose up -d --no-build --force-recreate webhook publisher
docker compose exec -T publisher python -m open_assets validate
scripts/open-assets doctor
```

YAML 语法检查不检查业务字段。重建后的 `validate` 必须成功；若服务无法通过健康检查，恢复备份并再次重建，不继续投递任务。不要把备份、任务记录或真实配置提交到 Git。降低额度前，应先检查现有占用；超限不会自动删除数据。

## Git 拉取

每次普通发布的两次 fetch 和对象恢复的 fetch 都使用同一套预算：

- `git_repository_bytes`：当前仓库缓存的逻辑文件字节上限，包含历史和未引用对象。
- `git_total_bytes`：发布端 `state_root/git` 下全部仓库缓存的合计预算。
- `git_fetch_bytes`：本次 fetch 相对开始时的目录增长预算，同时用作 Git 子进程单文件大小上限。它不是网络传输流量配额，也不是远端仓库配额。
- `git_min_free_bytes`：Git 所在文件系统必须保留的可用字节；图片 staging 仍有独立的容量预留检查。
- `git_timeout_seconds`：每条 Git 命令的最长运行时间，包括 fetch、导出与元数据命令；不是整个发布的总时限。
- `git_process_memory_bytes`：Linux 上继承到 Git/SSH 子进程的虚拟地址空间上限，不是 RSS 或所有进程的内存总和。macOS 开发环境不设置此项 rlimit；生产验收在 Linux 容器完成。
- `git_output_bytes`：捕获到内存的单条 Git 元数据输出上限。blob 直接写入暂存文件，不走这个捕获缓冲区。
- `scan_entries`：单次预算扫描的目录项上限，包含目录，超过时停止准入或拉取。

fetch 禁止递归子模块和自动 GC，限制 pack 线程数。新提交先写入临时引用，完成拉取并再次核查预算后，才更新 `refs/heads/source`。预算拒绝时，不切换图片，也不覆盖此前的发布引用。

目录容量是拉取前、运行中和完成后的周期检查。两次检查之间仍可能超出目录预算；单文件限制由 Linux 内核执行，但不能保证整个目录没有瞬时超量。需要严格隔离时，另设专用文件系统配额和容器总内存、进程数量限制。Gitea 服务端生成 pack 的资源消耗也需单独限制。

失败可能留下未引用对象或临时引用，这些文件继续计入后续预算。程序不自动 prune Git 历史，也不删除已有缓存来腾空间；历史可能仍是恢复来源。先检查故障和备份，再决定扩容、提高合理预算或在停 Worker 的维护窗口处理缓存。不要在运行中手工删除 `objects`。

## 队列准入

准入持有独立队列锁，Webhook 和手工 `publish` 走同一路径。新任务写盘前同时检查：

- 活动目录 `jobs` 的记录数量，包括 pending、运行中、已完成和失败任务。
- `jobs`、`job-archive`、`quarantine` 的合计文件记录数量。归档不会释放合计记录额度。
- 合计预留字节。每个文件至少计 64 KiB，较大异常文件按实际逻辑大小计入，为任务后续状态增长留出空间。隔离原因文件也计数；符号链接和特殊文件只按目录项计入，不跟随或打开。

这是一项保守的逻辑准入预算，不是实际磁盘占用显示，也不覆盖所有文件系统元数据、原子写临时文件和其他发布数据。队列目录本身仍需监控。清单、Git、恢复审计与 S3 对象有各自的容量管理边界。

同一仓库、SHA 和任务类型已有活动或归档任务时，直接返回原任务 ID；即使队列已经满，也不新增、不重置旧任务。满额时不丢弃最旧任务、不自动解除保护，也不重试失败任务。

Webhook 超限返回 HTTP 503，附 `Retry-After: 60` 和不含秘密的限制代码；它没有确认接收新任务。Gitea 是否自动重投取决于其配置，解除限制后应检查并重投失败事件。手工命令退出非零，输出 `resource_limit` 字段。Git 预算导致任务失败时，任务 JSON 记录 `ResourceLimit` 和限制代码，`status` 在原有四列后追加代码：

```sh
scripts/open-assets status all --key main-images
# 核对原因、调整额度或完成经确认的清理后：
scripts/open-assets retry <job-id>
```

`retry` 使用原任务，不新增记录。进程地址空间或单文件 rlimit 也可能表现为 Git 非零退出，即 `CalledProcessError`，不能只凭这一类型断定密钥失效。原始 Git stderr 不写入公开日志，避免暴露地址和访问参数。

活动记录较多时可按[本地保留策略](retention.md)归档符合条件的终态任务。合计满额时归档本身无助于释放额度；核实数据增长、保留需求和备份后再调整预算。当前不提供自动删除任务归档或隔离证据的命令。

## 普通发布的内存使用

Git blob 直接流入 staging 文件，之后以 1 MiB 块计算 SHA-256。S3 上传传入打开的文件流和明确的 Content-Length；已有对象与上传后的读回同样按最多 1 MiB 分块校验大小与 SHA-256，发现超出预期长度即停止，并在成功、失败路径关闭响应体。已有本地版本也使用分块的完整目录校验。

同一 SHA 的既有对象若内容不同，仍拒绝发布，不覆盖冲突内容。流式处理不会让多对象上传成为事务，发布失败仍可能留下部分对象。峰值内存还包括 Python、SDK、元数据清单、Git 子进程和系统缓存，不能理解为整个容器只使用 1 MiB。

独立大文件验收方法见 [e2e 说明](../tests/e2e/README.md)，结果见[验收记录](acceptance.md)。不要在业务桶或真实仓库中制造超限和内存耗尽测试。
