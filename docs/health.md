# 容器健康检查与受限恢复

`running` 只表示容器进程在运行。Docker 的 `healthy` 来自容器健康检查；健康失败本身不会触发 `restart: unless-stopped`。本项目把进程退出重启与持续不健康的处理分开：Docker 处理进程退出，宿主机定时器在限定范围内重启持续不健康的运行容器。

## 每个组件检查什么

- Gitea：本机 HTTP `/api/healthz`，启动宽限为 120 秒，避免正常初始化被当作失败。
- OtterIO：HTTPS `/otterio/health/ready`，验证内部 CA 和 `storage.internal` 主机名；启动宽限为 60 秒。
- OC：HTTPS 网页入口，验证内部 CA 与配置的存储管理域名；不使用跳过证书校验的选项。
- Nginx：本机 8081 的 `/__health`，与公开图片路径分开。
- Stargate：本机 `/healthz`。这不代替真实密码登录和会话交换验收。
- Webhook：本机 `/healthz`。返回存活不表示某次事件已经发布。
- Publisher：持久化 Worker 心跳、主循环进展与当前任务运行时间，不以 PID 存在作为唯一依据。
- Info：本机 `/healthz` 检查 HTTP 进程；快照的新鲜度由页面另行标识，重启网页不能修复采集配置错误。
- 现有 Traefik：保留原部署的 ping 健康检查。应同时确认其 Compose 声明保留 edge/auth 网络，不能只依赖临时 network connect。

主要探针每 20 秒执行，超时 5 秒，连续失败 3 次才标记 unhealthy，各组件另有启动宽限。现有 Traefik 使用它所属项目的检查周期。

Publisher 每 10 秒写一次心跳。空闲主循环最长允许 120 秒没有进展，心跳文件最长允许 60 秒未更新。单任务最长运行时间通过私有 `.env` 中的 `WORKER_MAX_JOB_SECONDS` 配置，默认 1800 秒，至少 60 秒；大仓库、慢链路部署应按实际发布时长调整，不能让合理的长任务被误判为挂起。

Worker 被终止时，文件任务和已上传对象仍可能处于中间状态。重启依赖现有的 running/images_prepared 重放与字节校验，不删除部分对象，也不保证跨系统回滚。进程持续崩溃或持久数据损坏仍需人工排查，见 [风险评估](risks.md)。

## 自动恢复的边界

恢复程序只匹配 Compose 项目 `asset-stack` 中的明确服务白名单，以及私有 `TRAEFIK_PROJECT` 指定项目中的 traefik 服务。无关项目不进入恢复范围。网页容器没有 Docker socket；仅受信任的宿主机运维程序有 Docker 管理权限。

恢复还要求：容器正在运行、没有被 Docker pause，Docker 健康状态持续 unhealthy 且被观察至少 30 秒。停止、暂停、starting、缺少健康检查的容器不会被自动拉起。每次动作前再次检查状态。

保护限制：同一服务重启后冷却 10 分钟、每小时最多尝试 3 次；全局 15 分钟最多尝试 3 次；一轮最多处理 2 个容器。失败尝试也计入预算。超出限制后保留故障供人工处理，不反复重启。额度约束的是这个恢复程序，不替代 Docker 自身的进程退出重启策略。

程序只执行容器重启，不重新初始化服务，不删除卷、对象、任务或证书，不修改 IAM、配置、仓库内容或数据库。一次重启命令成功不表示业务已恢复，仍应检查后续 healthy 状态和真实功能。状态型服务重启可能中断活动请求或写入，需要维护窗口时先设置下述标记。

## 安装与查看

在已经部署的服务器管理员 shell 执行。先把现有 Traefik 的实际 Compose 项目名写入私有 `.env` 的 `TRAEFIK_PROJECT`，不要假设它恰好叫 traefik。可从目标容器标签确认：

```sh
TRAEFIK_CONTAINER=your-traefik-container
docker inspect "$TRAEFIK_CONTAINER" \
  --format '{{index .Config.Labels "com.docker.compose.project"}}'
```

普通应用已经带健康检查；更新 Compose 后需重建相应容器，只有改磁盘文件或 reload Nginx 不会安装容器元数据中的健康检查。维护前先备份并暂停不适合中断的写入。

```sh
cd /app/open-assets
install -d -o root -g root -m 0700 /data/asset-stack/repair
python3 -m open_assets.repair
install -m 0644 config/systemd/open-assets-repair.service /etc/systemd/system/
install -m 0644 config/systemd/open-assets-repair.timer /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now open-assets-repair.timer
```

不带 `--apply` 仅报告计划，不执行重启。systemd 单元显式使用 --apply。恢复记录保存在 `/data/asset-stack/repair/state.json`，root 权限；Info 只读取服务名、时间、固定原因和结果，不显示容器完整配置。

```sh
docker compose ps
systemctl status open-assets-repair.timer --no-pager
journalctl -u open-assets-repair.service --since '30 minutes ago' --no-pager
```

维护或调试前：

```sh
cd /app/open-assets
touch private/maintenance.flag
systemctl stop open-assets-repair.timer
systemctl status open-assets-repair.service --no-pager
```

确认正在执行的恢复单元已结束，再进行维护。标记存在时，计划和后续动作都会跳过恢复。维护结束、服务检查通过后删除由本次维护创建的标记并启动 timer。不要删除其他操作者设置的维护标记。

恢复程序不能修复网络声明缺失、错误配置、过期信任、损坏数据库、坏任务文件或磁盘故障。看板上的重启记录用于排查，不应当作业务恢复的充分证据。异机监控、告警和备份仍需单独配置。

Publisher 自身现在会隔离无效任务并继续队列处理，详见 [异常任务隔离](operations.md#异常任务隔离)。这是应用层处理，与宿主机的容器重启机制不同；重启程序不会修复或重放隔离记录。

Worker 遇到同步对象恢复等维护操作持有的 Worker 锁时，非阻塞跳过该轮并继续刷新心跳。待处理任务会在锁释放后的轮次继续执行；健康不等于队列没有等待，也不证明恢复已经完成。不要因维护锁存在就移除锁或强制重启持有者。
