# Info 看板

Info 通过 Stargate 提供只读状态页面。先看组件、磁盘和证书，再按需要打开容量、保留、恢复或审计 Tab。页面不执行发布、删除和修复命令。

## 安装采集服务

在完成部署的服务器执行，先确认 `.env` 中的 `DATA_ROOT` 与实际目录一致。以下路径适用于默认配方；自定义路径需一起修改 systemd 的 WorkingDirectory、ExecStart、ConditionPathExists 和 ReadWritePaths。

```sh
cd /app/open-assets
sudo install -d -o root -g 20010 -m 0750 /data/asset-stack/info /data/asset-stack/info-refresh
sudo docker compose up -d --no-build info
for name in info objects audits resources doctor retention object-retention repair logrotate; do
  sudo install -m 0644 "config/systemd/open-assets-$name.service" "config/systemd/open-assets-$name.timer" /etc/systemd/system/
done
sudo install -m 0644 config/systemd/open-assets-info-refresh.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now open-assets-info-refresh.service
sudo bin/open-assets telemetry --root /app/open-assets
sudo systemctl enable --now open-assets-info.timer open-assets-objects.timer open-assets-audits.timer open-assets-resources.timer open-assets-doctor.timer
```

上述命令安装所有单元，只启用采集。对象清理账号和删除权限按对象保留文档另行准备。保留清理、健康恢复和日志轮转分别按[本地保留](retention.md)、[对象保留](object-retention.md)、[健康检查](health.md)和[日志](logging.md)启用。`doctor` 将采集与日志轮转列为必需项，自动修复和两个保留任务列为可选项。未启用可选项不报故障；已执行失败和缓存过期仍提示，见[部署诊断](doctor.md)。

首次安装后，各采集器按自己的启动延迟和间隔运行，页面可能暂时显示“尚未采集”。确认日志轮转已按其文档安装，再用 doctor 检查缺项；不要为了消除提示启用自动修复或实际清理。需要立即取样时可手动启动对应的采集 service，完成后再启动 `open-assets-info.service` 生成页面快照。

点击标题“运行状态”会重新加载页面；“刷新数据”会请求重新采集主机、容器、证书及有界本地容量信息，并在完成后重新获取快照。采集中按钮禁用；全局 10 秒冷却、采集锁和失败提示避免重复采集及将旧数据误报为新结果。对象数量、审计和诊断仍显示独立采集器的时间，不因按钮点击执行 S3 扫描、清理或修复。

Info 仍只读挂载快照，通过仅供 UID 20010 使用的 Unix socket 请求固定的采集动作；不挂载 Docker socket、不接受命令或路径参数。手动刷新 POST 需要同源和专用请求头，整个入口继续由 Stargate 认证保护。已有部署需安装上述独立采集服务、创建 socket 目录并更新 Info 挂载；未安装时按钮会明确报错，自动读取快照仍可用。

主机快照每轮结束后约 30 秒更新，页面每 30 秒读取一次。超过 120 秒会标记过期。查看采集故障：

```sh
sudo systemctl status open-assets-info.timer --no-pager
sudo journalctl -u open-assets-info.service --since '10 minutes ago' --no-pager
```

## 管理入口

Info 使用 `info-<节点名>.console.<基域>`。已有部署新增入口时，更新 `private/domains.json`、Stargate 回调名单和 Traefik 动态配置，保留原有密码与交换密钥。具体路由生成见[部署说明](deployment.md)。

匿名访问页面、`/api/status` 和 `/app.js` 都应跳转 Stargate。认证链使用精确的 `/_session_exchange` 与受保护的 `/__auth_return` 路径，不能将认证例外扩展为全局规则。HTTP 容器只读最终快照，不持有 Docker socket、存储凭据或 Publisher 状态目录。

## 健康、磁盘和证书

组件行显示运行状态、容器健康，以及配置过的 HTTP 探针结果。没有 HTTP 探针时不显示第三项。Publisher 心跳只说明工作循环存活；判断具体发布是否成功，还需查询对应任务。

Docker CPU 的 100% 表示一个逻辑核，主机 CPU 的 100% 表示整机。内存工作集、RSS 和主机可用内存口径不同。Docker RestartCount 不包含所有手工或修复程序发起的重启，另查看运维记录。

磁盘单独显示空间和 inode。容量阈值位于 `.env` 的 `INFO_DISK_*`、`INFO_INODE_*`；默认空间使用率 85% 提示、95% 严重，可用空间低于 5 GiB 提示、2 GiB 严重，inode 分别为 85% 和 95%。还会核对各仓库发布暂存预留。阈值错误或统计未知时，页面提示待确认。

证书分内网和 ACME 公网两列。内网证书由 Certs Maker 生成，CA 和叶证书有效期均为 3650 天，没有自动续期；公网证书由原 Traefik 续期。更新方法见[证书维护](upgrade.md)。

## 仓库容量、保留策略和清理结果

“仓库容量”展示版本数、完整 public/图片副本、未配对目录、本地占用、OSS 对象统计和最近清理结果。版本按 SHA 去重；目录存在不表示内容已校验。占用包括版本、Git 缓存、清单、暂存和清理暂存，不含共享队列、备份、Docker 镜像及日志。

本地占用按分配块估算，与逻辑文件字节不同，也不是保证可回收的空间。默认缓存 300 秒，配置、版本或清理记录变化时可提前刷新；扫描共享两秒与 50,000 项预算。失败保留上次值并标为旧数据。

清理结果区分预览、实际执行、锁忙、禁用及失败。服务正常退出也可能是跳过，不能只看 systemd 退出码判断删除了多少。先在 CLI 预览，再按[保留策略](retention.md)操作。

## OtterIO 对象统计

对象数量和逻辑字节按仓库前缀低频采集。上传中的部分版本也计入，分页不构成单一时刻的快照；逻辑字节不等于存储底层磁盘占用。

`open-assets-objects.timer` 使用现有 Publisher 列表权限，默认每 15 分钟采集。每轮共享 100 页、110 秒预算，每页最多 1000 个对象，列表请求最多每秒一次。未完成时保持续页，超过 24 小时重新扫描。页面刷新不调用 S3。

```sh
sudo systemctl status open-assets-objects.timer --no-pager
sudo journalctl -u open-assets-objects.service --since '1 hour ago' --no-pager
```

对象保留 Tab 显示当前策略、规划中的待清理版本和容量、最近尝试与最近完整预览/执行。规划过期或配置变化时需重新预览；实际删除以收据和执行结果为准。缺失、失败和跳过原因会单独提示。授权及执行方法见[对象保留](object-retention.md)。

## 恢复结果与状态核对

“恢复结果”显示每仓库最近启动的恢复：对象读回、本地副本校验、是否要求及完成显式回滚、失败阶段和不确定状态。报告中的成功仅描述当时的校验。后续核对或结案不会改写原恢复报告。

“状态核对”显示 `check-restore` 对对象、两份本地副本、current/published 和清理屏障的检查。页面显示核对当时目标是否在用，不持续校验当前版本；历史回滚结果请查看对应恢复记录。原恢复仍未结案或状态不确定时，核对通过后还需显式结案。

每个面板分别取最近报告，因此可能对应不同提交。已通过的旧核对若仅超过 24 小时或 current/published 正常切换，会显示灰色历史记录；它不证明当前状态，需要当前结论时可重新核对。失败、证据损坏、原报告/清单/范围变化仍保留待处理提醒；这不是持续的文件完整性检测。指定版本的调查使用 CLI：

```sh
sudo ./scripts/open-assets check-restore KEY FULL_SHA
```

操作选择见[命令速查](operator.md)，完整条件见[恢复](object-restore.md)、[核对](restore-check.md)和[结案](restore-close.md)。

### 提醒状态与处理

- **待处理**：仍有失败、不确定结果、证据变化或无法读取的记录，继续使用醒目颜色和页签提醒。快照过期时不能按旧结果认定已处理。
- **已处理（灰色）**：有匹配的后续核对、显式结案或人工确认拒绝提醒的证据。人工确认显示为“已人工确认无需处理”，不表示恢复已结案。匹配在宿主机内比较仓库、目标 SHA 与原报告摘要；其他提交、同一 SHA 的新恢复或更早的结案不能覆盖当前问题。不确定恢复仍要求显式结案，不能仅凭核对通过消除提醒。
- **历史记录（灰色）**：成功核对之后仅发生引用变化或记录过期，或者完整恢复本就无需结案。这不是一次新失败，也不等于问题已修复。原始时间和变化说明保留，需要当前结论时再核对。

分类与行颜色、页签及顶部摘要使用同一结果。已处理记录仍可查看，原失败报告不会被改写或删除。成功结案之后正常发布不要求重复结案；新的恢复、清单变化或无法比较现场时重新提醒。在线审计接近上限、缓存过期等独立问题不会因为某条记录结案而被隐藏。

已经核实无需处理的结案拒绝，可使用[人工确认 CLI](closure-acknowledgment.md)：默认预览，显式确认后单独保存审核原因与证据，并标灰；新尝试或证据变化后恢复提醒。这不替恢复结案，也不消除其他恢复、核对或审计问题。

原恢复报告不可用导致的结案拒绝会继续提醒。先查原报告与私有备份；若从未执行恢复，则无需结案。数据核对通过不能证明缺失报告已恢复，不应为消除提示创建新恢复、伪造报告或盲目重复结案。

每条待处理记录提供下一步建议：先核对对应版本；确认为对象或本地副本缺失后才恢复；不确定状态满足条件后显式结案；审计先校验和预览归档。页面不执行这些命令，也不建议删除锁、改写 current/published 或强制覆盖冲突。

命令中的 `FULL_SHA` 必须替换为该条报告对应的完整 SHA，不能直接用当前分支 HEAD。目标可在服务器 `${DATA_ROOT}/publisher/object-restores/<key>/<sha>.json`、`restore-checks/<key>/<sha>.json`，或 `restore-closures/<key>/<audit-id>/intent.json` 中核对。私人原因、摘要与目标身份不为关联展示而额外公开到 Info。

运维记录中，失败的自动修复只有在同一服务存在较晚的重启记录且当前新鲜采样健康时，才标为灰色已处理。仅看到容器 running 不足以消除失败提醒。旧压测错误不代表当前服务故障；先用 doctor 排查，再按性能文档安排限量复测。

## 审计容量缓存与实际归档结果

“结案与审计”保留独立结案结果，并展示在线、归档和待清理暂存的数量与占用、默认保护规划和最近实际归档摘要。原始报告、原因为私有材料，页面仅显示有限字段。

`open-assets-audits.timer` 默认每 15 分钟采集；普通主机快照只读取缓存和有界的操作摘要。归档后需要立即更新时：

```sh
sudo bin/open-assets audit-info --root /app/open-assets --force
sudo systemctl start open-assets-info.service
```

采集本身不归档文件。先用 `restore-audit-archive` 预览，未结案和不确定证据继续保护。参数、完整性校验及中断恢复见[审计文档](restore-audit.md)。

## Git 缓存与全局队列

“Git 与队列”展示发布端每仓库的 Git 缓存逻辑字节、预算和使用率，以及全局缓存、活动记录、合计记录与队列预留字节。合计 Git 占用包含未配置仓库残留缓存，不包含 Gitea 源仓库。

队列口径与准入一致：活动目录包含终态任务；合计还包括归档和隔离文件，每文件至少预留 64 KiB，较大文件按实际逻辑字节计入。页面分别显示实际字节和预留值；仅归档任务不释放合计额度。

默认每 900 秒测量，timer 每分钟检查是否需刷新。扫描最多 60 秒，目录项受 `resource_limits.scan_entries` 限制。采集不等待已被占用的 Worker/队列锁；测量期间新发布可能短暂等待，Git 扫描期间仍允许入队。锁忙下轮重试，其他失败保留旧值并标记。

`.env` 可配置 `INFO_RESOURCE_INTERVAL_SECONDS`（60–86400 秒）和 `INFO_RESOURCE_WARN_PERCENT`（1–99，默认 80）。达到提示比例时显示接近预算，达到 100% 显示达到或超过。预算变更后旧缓存失效。提示不修改准入额度、清理数据或发送通知。

```sh
sudo bin/open-assets resource-info --root /app/open-assets --force
sudo systemctl start open-assets-info.service
sudo systemctl status open-assets-resources.timer --no-pager
```

最近超限只保存本功能启用后、程序捕获的最近一次队列和 Git 限制代码、key 与时间。它是历史记录，不代表当前仍超限，也不能解释内核直接终止或事件写盘失败。主机快照只读取两个固定槽位，不扫描旧任务。

## 部署诊断

“部署诊断”页签展示 doctor 最近一次采样的分类结论、五个必需和三个可选定时器、最近执行结果、缓存年龄和排查提示。可选项未启用单独显示，不作为故障。存在待处理项、诊断过期或采集失败时，页签和顶部详情提示会提醒复核。

诊断由独立的 `open-assets-doctor.timer` 驱动。timer 每分钟检查是否到期，默认每 300 秒实际执行一次，服务最长运行 45 秒；超过时限由 systemd 结束整个服务进程组。该任务独立于每 30 秒的 Info 主机采集，不延长主机采集中的检查链。诊断任务不列入自身报告；其服务状态和诊断缓存时效单独展示，避免以自己的当前执行代替上次结果。

`.env` 的 `INFO_DOCTOR_INTERVAL_SECONDS` 可设为 60–86400 秒。超过两倍间隔视为过期；新鲜度按诊断完成时间计算。主机采集只读取固定大小、有字段过滤的 `doctor-cache.json`，再写入 `snapshot.json`；HTTP 仅读取快照，刷新页面或切换页签不会运行 doctor、访问 Docker/systemd 或遍历存储。

下表状态和年龄是**诊断采样时**的结果，并非当前实时状态。采集失败、被中断或结果过期时保留上次报告并标记旧结果；没有有效报告时显示不可用。诊断采集成功与检查通过分开：服务成功执行后，报告仍可能有待处理项。配置范围变化会使旧报告失效。

需要更新时，在服务器执行：

```sh
sudo bin/open-assets doctor-info --root /app/open-assets --force
sudo systemctl start open-assets-info.service
sudo systemctl status open-assets-doctor.timer open-assets-doctor.service --no-pager
```

`--force` 只忽略采集间隔，不绕过采集锁。以上操作仍只读诊断，不触发修复或清理。页面没有执行命令的按钮；按提示查看日志后人工处理。即时排查可直接运行 `sudo ./scripts/open-assets doctor`；完整规则见[部署诊断](doctor.md)。

## 数据位置与维护

过滤缓存位于 `${DATA_ROOT}/info/`，包括 `repositories-cache.json`、`objects-cache.json`、`audit-cache.json`、`resource-cache.json`、`doctor-cache.json`。缓存可重建，原始恢复、清理收据、审计及 `publisher/resource-limit-events.json` 应纳入私有备份。HTTP 只读取过滤后的 `snapshot.json`。

维护前按[维护窗口](operations.md#维护窗口)设置 `private/maintenance.flag`，停止相关 timer 和已经运行的 service，并暂停人工写操作。标记只被部分维护程序检查，不能暂停所有采集、在途任务或人工确认。`--force` 只绕过统计间隔，不绕过锁或扫描预算。数据过期先检查对应服务，不通过重跑初始化或取消删除保护解决。

主机用量趋势显示 CPU 整机使用率（100% 为全部逻辑核）、内存使用率和 1/5/15 分钟系统负载，提供最近 1、6、24 小时视图及样本均值、最高值。负载虚线表示当前逻辑核数，不是故障阈值。浏览器切换范围只读已有样本，不触发主机采集。

历史由现有约 30 秒主机采集器写入 DATA_ROOT/info/host-history.json，按 30 秒桶保存最近 24 小时，最多 2881 个数值样本，重启后保留；手动刷新在同一桶内更新最近样本。只记录时间、CPU、内存和负载，不记录环境变量、请求或凭据。尚无历史、指标缺失、采集间断和过期会明确显示，不补造零值、不跨两分钟以上的间断连接曲线。概要样本不是连续监测或瞬时峰值，部署前的数据不会补造。

本地容量统计识别直接快照内的共享图片视图，兼容视图与物理字节分别显示，不把缺少第二目录的新版本当作未配对。扫描仍受两秒时限和有限目录项预算约束，按真实 inode 去重统计已分配字节；超限会保留旧样本并明确标注，不能将旧值当作当前值。

大仓库容量扫描与 30 秒主机采样分离：安装并启用 open-assets-storage-info.timer，每五分钟运行一次受 CPU 25%、256 MiB、80 秒扫描和目录项预算限制的采样；主机刷新仅读已完成缓存，手动刷新另外提交固定后台采样请求，不在请求中遍历全部历史目录。部署和回退时分别启用或停用该计时器，历史数据文件保留。

容量扫描最多检查 100 万个条目，以覆盖保留版本和单个待用硬链接视图的目录元数据；独立采样进程的 CPU、内存和 80 秒扫描时间预算保持不变。达到预算仍显示旧数据和过期状态，不将未完成遍历当作完整容量。

## 发布阶段耗时

“运维记录”展示最近最多 100 条活动任务记录的发布阶段，以及各阶段成功样本的 p50/p95。任务归档后不在这个窗口内；这不是永久历史。旧记录缺少的细项显示为缺失，不补零。刷新数据会重新采集这些记录，不读取图片内容，也不执行发布或清理。

排队从任务入队算起；“入队至完成”不包含客户端 Git 推送前的连接和传输，也不是公网可见性的测量。每次执行单独记录 Git 拉取、导出与核对、对象上传、快照落盘、远端确认与切换和证据保存；失败和被替代任务保留已完成阶段，成功统计排除这些任务。

导出细项包括规划、导出与安全校验、并行对象核对以及等待核对。对象核对与导出重叠，细项不能直接相加。原子切换与切换前的远端确认另列，便于区分磁盘开销与网络等待。
