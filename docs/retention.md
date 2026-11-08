# 本地历史版本保留与清理

## 清理范围

本功能清理各仓库本地的 `releases/<sha>/` 完整 public 副本和 `releases/images-<sha>/` 图片副本，并把较旧的 published/superseded 任务移到归档。**本地清理不删除 OSS 对象**，仅运行本功能时，已分享的附件 SHA URL 继续保留；不修改 Git 仓库、Gitea 数据、证书、备份、清单、异常任务隔离目录或普通 staging 目录。

对象需要单独治理时，使用[对象历史保留](object-retention.md)。它默认只预览，不继承本地 enabled；实际删除可能使旧附件链接失效。两类清理共享保护引用，也保护在途/失败任务的 previous，避免清理其恢复引用。

清单和归档任务仍会占用少量空间，Git 历史及 S3 对象也会继续增长。这是本地版本保留机制，不是整个部署的磁盘配额。发布前仍需检查空间，并配置独立容量告警和备份。没有可清理版本时，即使磁盘紧张也不会突破保护规则强制删除。

## 配置

在私有 `repositories.yaml` 顶层设置默认值，仓库内同名 retention 配置可覆盖单项。示例默认关闭实际清理：

```yaml
retention:
  enabled: false
  keep_last: 5
  keep_days: 30
  min_age_hours: 24
  rollback_versions: 2
  pinned_shas: []
  archive_jobs_after_days: 30
  max_delete_versions: 10
  max_delete_bytes: 268435456
  max_archive_jobs: 100
repositories:
  - key: main-images
    # 保留该仓库其他身份、远端、源目录及挂载设置。
    retention:
      enabled: true
      keep_last: 10
      pinned_shas:
        - 0123456789abcdef0123456789abcdef01234567
```

不要把这里省略的仓库字段直接当作完整配置。修改已有私有配置前备份，执行 `validate`；使用原子替换单文件配置时，按部署方式重建 Webhook/Publisher，使 bind mount 指向新文件。新增仓库 CLI 会保留全局策略和已有仓库覆盖值。

保留条件取并集：

- current 指向的版本必须保留，published 记录必须与其一致。
- `rollback_versions` 保留最近离开的不同版本，默认 2 个。发布和手工回滚都会在切换前记录保护；失败的切换可能多留版本。也会从活动目录中当前版本对应任务的 previous 引用补充保护，以兼容升级前记录；因此最终保留集合可能多于该数量。
- `pinned_shas` 是额外固定保留的完整 SHA。需要长期保留的回滚点应明确列入；滚动回滚保护不能代替长期固定。
- pending、running、images_prepared 和 failed 任务引用的 SHA 保留。failed 需人工重试或处理，不会自动过期删除。
- 最近 `keep_last` 个本地版本保留，最小值为 1；排序中的当前版本和回滚点也包含在这一数量中，其他保护条件再与该集合取并集。
- 未达到 `keep_days` 或 `min_age_hours` 的版本保留。时间取目录、文件、清单及有效任务时间中的较新值；再次发布或回滚会刷新版本目录时间，使近期使用的旧版本获得新的保留窗口。它不是 Git author/committer 时间。
- 缺少清单的版本保留；没有 current 引用时，现有本地版本全部保留。无效任务、损坏的引用、符号链接、非普通版本文件或 current/published 不一致会使本轮停止，排查后重新预览。

只有不在任何保护集合、同时达到时间窗口的版本才进入候选。`keep_days` 可设为 0，最小安全间隔 `min_age_hours` 至少为 1。禁止用 0 关闭保护。保留数量不是“最多只剩 N 个版本”的保证。

`max_delete_versions` 和 `max_delete_bytes` 分别限制每仓库每轮新移走版本的数量及文件逻辑字节数，默认 10 个 / 256 MiB。暂存区删除也单独受同样预算限制，过大的版本可能需要提高预算后才能清理。逻辑字节不等于实际磁盘块占用，已打开的文件也可能延后释放空间。

`archive_jobs_after_days` 仅对未受保护的 published/superseded 任务生效，同时考虑创建、结束及文件更新时间；`max_archive_jobs` 默认每仓库每轮 100 份。归档保留原始 JSON 和任务身份，用于去重，不自动删除。

## 预览和执行

在服务器 `/app/open-assets` 下执行，普通运维账号为 Docker 命令加 sudo：

```sh
# 默认只预览，输出保护原因、候选版本及拟归档任务。
docker compose exec -T publisher python -m open_assets.retention --key main-images

# 重新读取状态并计算，只有 enabled=true 的仓库才实际清理。
docker compose exec -T publisher python -m open_assets.retention --key main-images --apply

# 不指定 key 时检查所有已配置仓库。
docker compose exec -T publisher python -m open_assets.retention --apply
```

预览不移动或删除版本、任务；获取锁可能创建锁文件。执行不会使用过期预览文件作为删除清单，每次都重新判断。建议先备份、预览，再启用单个仓库，确认回滚点及候选范围后执行。

返回 `busy` 表示发布、入队或其他清理正在持锁，本次不清理。返回 `blocked` 且非零退出表示遇到不安全状态或 I/O 错误；不输出私有文件内容。规划最多读取 10,000 份活动任务，保留必要字段参与计算，超限会停止而不是继续消耗内存。规划阶段另设 30 秒检查预算，超时在移动版本前退出；该检查不能中断阻塞的内核 I/O。不要反复重启进程或手工修改 current 来跳过检查。

执行后的最近完整全局结果保存在 `state_root/retention-last.json`，下次覆盖，不无限累计。已启用仓库另写 `state_root/retention-results/<key>.json` 的计数摘要；单仓库执行不会覆盖其他仓库摘要，禁用、忙碌或失败不会冒充该仓库的完整成功。定时服务结果进入宿主机 journal，使用已有日志保留策略；手工执行时需自行保存输出。

## 原子移动、中断与归档

清理与发布/回滚共用 Worker 锁，并在检查和移动时持有队列锁，阻止引用在这一窗口内改变。锁采用非阻塞获取。规划会读取元数据，短暂延迟入队；版本移走后释放这两把锁，删除暂存内容期间可继续发布。独立清理锁防止多个清理进程同时删除同一暂存目录。

可清理版本先在同一文件系统内移动到仓库下 `.retention-trash/<随机标识>/`，保存 key/SHA 收据，然后删除。只有成功移走的内容才会被删除。进程中断后，重试只删除有效收据下的 full/images 目录，不按旧收据去移动 releases 中后来重建的同 SHA 目录；部分移动留下的另一半会在新一轮重新判断。无效或未知暂存内容停止删除并留待人工核对。

本地版本清理成功后，普通 `rollback-images` 无法直接回到已清理版本。需要恢复时，可使用 [restore-objects --with-local](object-restore.md#重建本地-public--图片副本) 从 Git 重建完整快照和图片副本，或从可信备份恢复。默认不切换当前版本；完整校验后可显式回滚。保留 OSS 对象本身不会自动重建本地版本。需要可随时回滚的 SHA 应先固定保留。

终态任务移到 `state_root/job-archive/<任务ID前两位>/<任务ID>.json`。enqueue 同时检查活动目录和归档，所以重复 Webhook 不会重新创建任务；显式人工发布是另一种任务身份，仍沿用原去重规则。查看方式：

```sh
sudo ./scripts/open-assets status
sudo ./scripts/open-assets status archived
sudo ./scripts/open-assets status all
```

`status` 默认只列活动目录。归档不是删除去重记录，也不等于重新发布；不要把归档文件移动回 jobs。

## 定时运行与暂停

可安装 `config/systemd/open-assets-retention.service` 和 `config/systemd/open-assets-retention.timer`。定时器以开机后 20 分钟为首次触发点；启用时若已超过该时间，可能立即执行。之后每 6 小时执行，并有最多 15 分钟随机延迟；仅清理 enabled=true 的仓库。它使用宿主机 Docker 权限调用现有 Publisher，不给应用容器挂载 Docker socket，也不增加 S3 删除权限。

```sh
sudo install -m 0644 config/systemd/open-assets-retention.service config/systemd/open-assets-retention.timer /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now open-assets-retention.timer
sudo systemctl start open-assets-retention.service
sudo journalctl -u open-assets-retention.service --since=1day --no-pager
```

已有维护标记时服务不会启动；标记不能停止已经运行的清理。计划恢复备份或手工维护前，先设置维护标记，再停止 timer 和 service，并确认没有在途任务或手工清理进程；完整步骤见[维护窗口](operations.md#维护窗口)。停止 timer 不会自动停止已启动任务。

```sh
sudo systemctl stop open-assets-retention.timer open-assets-retention.service
# 检查维护与恢复步骤后再重新启用 timer。
```

中断可能留下待删除暂存区或已归档任务，属于可重试状态；它不具备数据库、文件系统与 S3 的联合事务或整机断电保证。清理前的快照和原始业务备份应由独立策略保留。

Info 看板可查看本地版本数、占用、保留策略及最近完整清理摘要，带独立的统计时间和采集失败提示；占用不包含 OSS 总量。详见 [看板口径与容量提示](info.md#仓库容量保留策略和清理结果)。
