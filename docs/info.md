# Info 看板

Info 通过 Stargate 提供只读状态页面。先看组件、磁盘和证书，再按需要打开容量、保留、恢复或审计 Tab。页面不执行发布、删除和修复命令。

## 安装采集服务

在完成部署的服务器执行，先确认 `.env` 中的 `DATA_ROOT` 与实际目录一致。以下路径适用于默认配方；自定义路径需一起修改 systemd 的 WorkingDirectory、ExecStart、ConditionPathExists 和 ReadWritePaths。

```sh
cd /app/open-assets
sudo install -d -o root -g 20010 -m 0750 /data/asset-stack/info
sudo docker compose up -d --no-build info
for name in info objects audits resources doctor retention object-retention repair logrotate; do
  sudo install -m 0644 "config/systemd/open-assets-$name.service" "config/systemd/open-assets-$name.timer" /etc/systemd/system/
done
sudo systemctl daemon-reload
sudo python3 -m open_assets.telemetry --root /app/open-assets
sudo systemctl enable --now open-assets-info.timer open-assets-objects.timer open-assets-audits.timer open-assets-resources.timer open-assets-doctor.timer
```

上述命令安装所有单元，只启用采集。对象清理账号和删除权限按对象保留文档另行准备。保留清理、健康恢复和日志轮转分别按[本地保留](retention.md)、[对象保留](object-retention.md)、[健康检查](health.md)和[日志](logging.md)启用。`doctor` 将采集与日志轮转列为必需项，自动修复和两个保留任务列为可选项。未启用可选项不报故障；已执行失败和缓存过期仍提示，见[部署诊断](doctor.md)。

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

“状态核对”显示 `check-restore` 对对象、两份本地副本、current/published 和清理屏障的检查。页面显示当前使用的版本；历史回滚结果请查看对应恢复记录。核对通过后仍需手动结案。

每个面板分别取最近报告，因此可能对应不同提交。超过 24 小时或发现相关证据变化会提示重新核对；这不是持续的文件完整性检测。指定版本的调查使用 CLI：

```sh
sudo ./scripts/open-assets check-restore KEY FULL_SHA
```

操作选择见[命令速查](operator.md)，完整条件见[恢复](object-restore.md)、[核对](restore-check.md)和[结案](restore-close.md)。

## 审计容量缓存与实际归档结果

“结案与审计”保留独立结案结果，并展示在线、归档和待清理暂存的数量与占用、默认保护规划和最近实际归档摘要。原始报告、原因为私有材料，页面仅显示有限字段。

`open-assets-audits.timer` 默认每 15 分钟采集；普通主机快照只读取缓存和有界的操作摘要。归档后需要立即更新时：

```sh
sudo python3 -m open_assets.audit_info --root /app/open-assets --force
sudo systemctl start open-assets-info.service
```

采集本身不归档文件。先用 `restore-audit-archive` 预览，未结案和不确定证据继续保护。参数、完整性校验及中断恢复见[审计文档](restore-audit.md)。

## Git 缓存与全局队列

“Git 与队列”展示发布端每仓库的 Git 缓存逻辑字节、预算和使用率，以及全局缓存、活动记录、合计记录与队列预留字节。合计 Git 占用包含未配置仓库残留缓存，不包含 Gitea 源仓库。

队列口径与准入一致：活动目录包含终态任务；合计还包括归档和隔离文件，每文件至少预留 64 KiB，较大文件按实际逻辑字节计入。页面分别显示实际字节和预留值；仅归档任务不释放合计额度。

默认每 900 秒测量，timer 每分钟检查是否需刷新。扫描最多 60 秒，目录项受 `resource_limits.scan_entries` 限制。采集不等待已被占用的 Worker/队列锁；测量期间新发布可能短暂等待，Git 扫描期间仍允许入队。锁忙下轮重试，其他失败保留旧值并标记。

`.env` 可配置 `INFO_RESOURCE_INTERVAL_SECONDS`（60–86400 秒）和 `INFO_RESOURCE_WARN_PERCENT`（1–99，默认 80）。达到提示比例时显示接近预算，达到 100% 显示达到或超过。预算变更后旧缓存失效。提示不修改准入额度、清理数据或发送通知。

```sh
sudo python3 -m open_assets.resource_info --root /app/open-assets --force
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
sudo python3 -m open_assets.doctor_info --root /app/open-assets --force
sudo systemctl start open-assets-info.service
sudo systemctl status open-assets-doctor.timer open-assets-doctor.service --no-pager
```

`--force` 只忽略采集间隔，不绕过采集锁。以上操作仍只读诊断，不触发修复或清理。页面没有执行命令的按钮；按提示查看日志后人工处理。即时排查可直接运行 `sudo ./scripts/open-assets doctor`；完整规则见[部署诊断](doctor.md)。

## 数据位置与维护

过滤缓存位于 `${DATA_ROOT}/info/`，包括 `repositories-cache.json`、`objects-cache.json`、`audit-cache.json`、`resource-cache.json`、`doctor-cache.json`。缓存可重建，原始恢复、清理收据、审计及 `publisher/resource-limit-events.json` 应纳入私有备份。HTTP 只读取过滤后的 `snapshot.json`。

维护前设置 `private/maintenance.flag`，停止相关 timer 和已经运行的 service；标记只阻止新任务启动。`--force` 只绕过统计间隔，不绕过锁或扫描预算。数据过期先检查对应服务，不通过重跑初始化或取消删除保护解决。
