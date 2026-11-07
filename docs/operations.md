# 运行维护

以下命令在服务器管理员 shell、`/app/open-assets` 目录执行。使用普通运维账号时，为 Docker 命令加 sudo。

## 查看状态和失败

```sh
docker compose ps
docker compose exec -T publisher python -m open_assets status
docker compose logs --tail=100 publisher webhook
```

status 默认列活动目录，`status archived` 列归档，`status all` 列两者。依次输出有效任务的 ID、仓库 key、SHA 和状态；遇到坏记录只报告固定错误类型并跳过，不修改文件。任务 JSON 位于 `/data/asset-stack/publisher/jobs/`，失败时只保存异常类型，不保存完整请求或秘密。日志为空不代表没有任务，也不代表发布成功。

容器健康与最近的受限恢复动作可在 [Info](info.md) 查看。自动重启有作用域、冷却和次数上限，不会替代坏配置、坏任务或存储数据的处理。维护和有意暂停服务前按 [健康恢复说明](health.md) 设置 maintenance.flag 并暂停恢复定时器，确认在途恢复已结束。

failed 任务需人工重试：

```sh
docker compose exec -T publisher python -m open_assets retry JOB_ID
```

命令只接受完整的失败任务 ID。重复投递相同 Push 不会重置失败任务。running 或 images_prepared 遗留任务会在 Worker 重启后重新处理；自动任务若已不是分支头，则变为 superseded。

## 手工发布与图片回滚

```sh
docker compose exec -T publisher python -m open_assets publish main-images FULL_COMMIT_SHA
docker compose exec -T publisher python -m open_assets rollback-images main-images FULL_COMMIT_SHA
```

publish 创建人工任务，提交必须在配置分支的历史中，但可以不是最新提交。发布历史 SHA 会重新选择当前图片版本，不应当作无副作用的补查命令。

rollback-images 使用已有本地清单和版本文件，校验后切换 current，并更新当前发布记录；它不创建新的发布任务，不改 Git 分支，不删除或回退 OSS 附件。目标本地版本或清单缺失时不能直接回滚。

回滚同时校验完整 public 快照与实际提供服务的 `releases/images-<sha>/` 副本，包括文件集合、类型、大小和 SHA-256；拒绝符号链接及缺失、额外或损坏的文件。切换后再次校验，切换或校验异常时尝试恢复旧链接。不自动重建损坏副本；先从可信备份恢复，不要通过手工改清单绕过校验。仍需通过公开 URL 验证内容；文件系统故障或并发管理员写入不在原子保证范围内。

计划维护时可停止接收器与 Worker，避免新任务紧接着覆盖回滚：

```sh
docker compose stop webhook publisher
# root shell 通过临时运行的同一应用镜像执行回滚：
docker compose run --rm --no-deps publisher rollback-images main-images FULL_COMMIT_SHA
docker compose up -d --no-build webhook publisher
```

停止接收器期间暂停 Push；窗口内失败的 Hook 需重投或按 SHA 补发。重新启动后，已有待处理任务仍会执行，必要时先核对队列。不要直接编辑任务或 current 来跳过这些检查。

## 备份

至少包含：OtterIO 对象及 IAM/config、Gitea 数据库与仓库、发布任务和清单、私有凭据及 CA、Traefik 配置和 ACME 状态。缺少解密或访问凭据的数据副本可能无法恢复。

`scripts/backup.sh` 只打包应用 private、.env 和数据目录，不停止服务，也不包含 Traefik；它不能单独视为在线数据库的一致性备份。

`scripts/snapshot.sh` 停止 webhook、publisher、OC、Gitea 和 OtterIO 后归档应用、数据及指定 Traefik/ACME 目录，再尝试启动固定的服务列表。它不记录这些服务原先是否停止，执行前应确认该行为符合维护计划：

```sh
TRAEFIK_CONFIG_ROOT=/path/to/traefik-project \
ACME_ROOT=/path/to/acme \
BACKUP_ROOT=/data/private-backups/open-assets \
sh scripts/snapshot.sh
```

这会造成短暂的 Git、存储和管理服务中断，已有本地图片仍由 Nginx 提供。Traefik 不会被脚本停止；ACME 若可能并发更新，需要另行安排一致性保存，不能把归档校验等同于整个系统的原子快照。

归档读取和 SHA-256 校验只证明文件可读、传输未变。恢复演练还要检查数据库、对象、IAM、证书及 SSH 主机密钥，确认服务能以正确权限读写。备份应加密复制到独立设备；同机目录不能覆盖整机或磁盘故障。项目没有默认安装定时备份和异机复制任务。

## 更新代码或镜像

先记录现有提交和镜像摘要，备份私有配置并查看待更新差异。不要通过重复 prepare-private.py 或 initialize-services.py 更新已有部署，也不要用 docker compose down -v 作为常规更新步骤。

应用源码改动后构建 publisher 镜像，再用同一个镜像重建 publisher/webhook；只修改宿主机管理 CLI 时无需重建运行容器。使用已审阅的固定版本，分别验证发布、读取及管理登录。

OtterIO/OC 升级需要核对配对版本、数据兼容范围和恢复步骤。镜像能启动不等于数据可安全降级。更换服务前保留旧程序和一致性数据副本。

## 容量与证书

检查磁盘空闲、Git 缓存、对象历史、本地完整版本、图片副本和临时目录。发布前的空间检查按 max_bytes 上限预留；临时失败数据不会自动回收。手动清理前核对 current、回滚目标和任务引用，先保留可恢复副本。

公网证书续期由原 Traefik 管理，内部证书需要另行维护。日志、备份、接入记录和已渲染配置均按私有资料保存，不上传公开仓库或工单。

日志容量、压缩和保留设置见 [日志管理](logging.md)。

## 异常任务隔离

Worker 对每份任务最多读取 64 KiB，拒绝符号链接、非普通文件、无效 JSON/字段和身份不匹配的记录。有效 pending、running、images_prepared 任务继续按创建时间处理；正常发布失败仍进入 failed，不属于坏记录隔离。

隔离记录保存在 `/data/asset-stack/publisher/quarantine/<随机标识>/`：`record` 保留原始文件或原始文件系统条目，`reason.json` 记录原文件名、时间和异常类型。不要跟随隔离的符号链接或尝试把 FIFO 当文本读取。查看容器日志中的 `job-quarantined`、`job-quarantine-failed`、`job-state-write-failed`、`queue-io-failed`，检查私有隔离目录并备份。当前没有默认外部告警或自动清理。

不要把未经修复的记录移回 jobs，否则会再次隔离。先确认根因（磁盘、人工编辑或恢复），然后核对固定仓库和完整 SHA；需要重新发布时使用 `publish KEY FULL_COMMIT_SHA` 创建有效人工任务。该命令可以切换到历史版本，必须先确认预期版本。隔离不等于任务发布失败，也不会自动撤销已上传对象或现有图片。

## 历史版本保留

本地版本保留、固定回滚点、终态任务归档及可选定时器见 [历史清理](retention.md)。默认只预览，策略显式启用后才清理；不删除 OSS 对象。计划维护时一并暂停清理 timer/service。

## 已清理对象恢复

使用[restore-objects](object-restore.md)从原仓库提交和可信清单恢复版本对象；已完成或归档的手工任务不阻止恢复。不要删除任务去重记录来触发重放。恢复后仍需分别确认对象链接、当前图片指向和本地回滚副本状态。

本地副本也已清理时，使用 restore-objects KEY SHA --with-local；恢复后默认保持 current。需要切换时可组合 --with-local --rollback，在同一维护锁内校验对象与双副本后激活。中断或失败按恢复报告分别核对 S3、本地安装和 current/published，不把部分完成当作事务回滚。
