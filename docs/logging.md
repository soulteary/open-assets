# 日志保存、压缩与轮转

## 容器日志

Compose 显式使用 Docker `local` driver：每文件 `10m`、最多 3 个文件，`compress: "true"`。Webhook、Publisher、Info、Nginx、OtterIO、OC、Stargate 和 Gitea 共用这一策略。Gitea 显式使用 console 日志，避免另存一份无限增长的文本日志；Nginx 关闭访问日志，错误日志输出到 stderr。

单容器轮转窗口约 30 MB（压缩前），八个应用约 240 MB；包括独立 Traefik 则约 270 MB。该值不是整个 Docker 数据目录的硬配额，轮转过程、元数据和解压读取会产生额外空间。超过窗口的旧日志会被删除；需要长期审计时，应另接带保留策略的集中日志服务。

查看方式不变：

```sh
cd /app/open-assets
docker compose logs --tail=100 publisher webhook
docker compose logs --since=1h gitea
```

修改 logging 后必须重建相应容器，普通 restart 不生效。不需要重启整个 Docker daemon。独立 Traefik 应在其现有 Compose 文件中设置同样的 logging；保留原镜像、网络、挂载、证书和凭据，先验证合并后的 `docker compose config`，再只重建 traefik 服务。

```yaml
logging:
  driver: local
  options:
    max-size: "10m"
    max-file: "3"
    compress: "true"
```

使用 `docker inspect --format '{{json .HostConfig.LogConfig}}' CONTAINER` 核对实际容器配置。不要对 `/var/lib/docker` 的日志执行 logrotate、truncate 或手工 gzip，Docker 自己管理这些文件。行为依据见 [Docker local driver](https://docs.docker.com/engine/logging/drivers/local/)。

## 宿主机 journal

`config/journald/90-open-assets.conf` 是宿主机级配置，会影响同机其他服务。专用服务器可安装为 `/etc/systemd/journald.conf.d/90-open-assets.conf`；共享服务器先评估原有策略和审计要求。

配置启用压缩，持久 journal 目标上限 256 MiB、为文件系统保留 2 GiB，单文件 32 MiB；运行时 journal 上限 64 MiB、保留 64 MiB；保留时间 14 天，单文件时间窗口 1 天。journald 清理归档文件，活动文件和其他文件占用可能使实际值暂时超过目标；保留空间设置也不能阻止其他程序写满磁盘。压缩针对超过阈值的 journal 数据对象，不保证每条短消息都压缩。

安装后重启 `systemd-journald`，使用 `systemd-analyze cat-config systemd/journald.conf` 检查最终配置，`journalctl --disk-usage` 查看占用。无需为降低当前用量强制 vacuum 删除历史。参考 [journald.conf](https://www.freedesktop.org/software/systemd/man/latest/journald.conf.html)。

## 私有部署和构建日志

`config/logrotate/open-assets-private` 只匹配 `/app/open-assets/private/updates/*/*.log`。每天轮转非空文件，超过 10 MiB 时可提前轮转，保留最多 7 份，轮转时清理超过 14 天的归档，压缩并延迟一轮压缩活动副本。`copytruncate` 避免影响仍持有文件描述符的临时构建进程，但复制和截断之间可能丢失极少量日志，不应用于要求无损的审计记录。

安装和检查：

```sh
sudo install -m 0644 config/logrotate/open-assets-private /etc/open-assets-logrotate.conf
sudo install -m 0644 config/systemd/open-assets-logrotate.{service,timer} /etc/systemd/system/
sudo logrotate --debug /etc/open-assets-logrotate.conf
sudo systemctl daemon-reload
sudo systemctl enable --now open-assets-logrotate.timer
sudo systemctl start open-assets-logrotate.service
sudo systemctl status open-assets-logrotate.timer --no-pager
```

独立 timer 每小时检查一次，使用自己的 state 文件。10 MiB 是检查时的触发阈值，不是文件硬上限；极高日志速率下，两次检查间仍可能增长。首次安装、修改路径或停用 timer 后需重新核对。活动文件仍是原权限；归档可能包含敏感输出，整个 private 目录必须限制访问。

不要扩展成 `/data/**/*.log`：Gitea 队列等目录中的 `.log` 是数据库文件，不是可轮转文本。任务、发布清单、对象、备份和证书也不属于本规则，容量与保留策略需分别管理。持续监测 Info 磁盘和 inode 使用情况；轮转不能替代容量告警。

## 2026-11-07 实际验收

九个容器实际配置均为 local / 10m / 3 / compress=true，且恢复 healthy。公开图片与附件读取及管理入口认证检查通过。journal 配置加载成功，私有日志 timer 已启用，首次正常运行成功。另用隔离的无敏感临时文本验证 logrotate 轮转、延迟压缩和 gzip 解压内容一致；隔离 Docker 测试也确认超出保留窗口的旧行被淘汰、最新行仍可读取。未强制轮转应用数据库或删除备份。
