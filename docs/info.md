# Info 运行看板

默认地址为 `https://info-node-name.console.lab.dev`。以私有 `domains.json` 中的实际地址为准。所有页面、静态资源和 `/api/status` 都经过 Stargate ForwardAuth；会话交换与认证落地页沿用精确管理主机路由。

## 展示内容与数据边界

看板展示主机 CPU 总利用率、逻辑核数、可用内存、Swap、系统负载与运行时间；组件的运行状态、Docker 健康状态、可用的 HTTP/TLS/心跳探测、CPU、内存工作集和重启次数；磁盘可用容量与 inode；内部 CA/叶证书及公网证书的到期时间。如果存在限量压测结果，还会展示请求数、错误数、吞吐和延迟分位数。

磁盘区独占一行，证书区在桌面宽度下分为内网与 ACME 公网两列，小屏幕顺序排列。顶部“认证状态”进入配置的 Stargate 认证主机，仅显示当前认证状态，不是退出按钮；不接收请求参数指定的返回地址。恢复记录仅展示固定字段，是否成功恢复仍以组件当前健康状态为准。

Docker CPU 的 100% 表示使用一个逻辑核；主机 CPU 则以整机为 100%，两者不能直接混用。内存工作集不是进程 RSS，也不包含主机上全部文件缓存。磁盘 `/` 与数据目录位于同一文件系统时只显示一项，不能把同一容量加两次。

宿主机 collector 有受信任运维权限，读取 Docker 状态、Linux `/proc` 和公开证书，生成明确字段的 JSON 快照。它不输出环境变量、密码、SSH 密钥、容器完整 inspect、主机名、公网 IP 或完整文件路径。版本镜像引用和系统版本属于运行信息，仍只供管理入口访问。

info 容器以非 root 身份运行，根文件系统只读，只有快照目录的只读挂载；不挂载 Docker socket、宿主机 `/proc`、private 或证书私钥。它提供固定读取路由，不提供 PHP 探针中的命令执行、文件管理、端口扫描或在线性能测试功能。

## 启动采集

完成部署、DNS 和 Traefik 接入后，在服务器管理员 shell 执行：

```sh
cd /app/open-assets
install -d -o root -g 20010 -m 0750 /data/asset-stack/info
docker compose up -d --no-build info
python3 -m open_assets.telemetry --root /app/open-assets
install -m 0644 config/systemd/open-assets-info.service /etc/systemd/system/
install -m 0644 config/systemd/open-assets-info.timer /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now open-assets-info.timer
systemctl status open-assets-info.timer --no-pager
```

固定路径与 Compose 项目限制仍适用。采集每次结束后约 30 秒再运行，页面每 30 秒读取一次；超过 120 秒的快照会标记过期。采集失败保留旧快照，不能把旧快照当作实时健康证明。定时器的状态与错误通过 systemd 日志检查：

```sh
journalctl -u open-assets-info.service --since '10 minutes ago' --no-pager
```

`/healthz` 只表示 info HTTP 进程存活。Publisher 的健康心跳不证明最近任务成功；Traefik 的 ping 不等于所有入口配置正确。判断具体发布还应使用任务状态与公开字节验证。各探针与自动恢复边界见 [健康检查](health.md)。

## 已有部署增加入口

先备份应用与现有 Traefik 私有配置，按 [升级说明](upgrade.md) 更新源码。根据 `.env` 中的节点与基域生成 info 地址，追加到 `private/domains.json` 和 Stargate 的 `CALLBACK_ALLOWED_HOSTS`，保留原有域名、密码与交换密钥。重新生成现有 Traefik 的动态文件，重建 Stargate 和 info，再安装采集定时器。

四个受保护管理主机都必须包含精确 `/_session_exchange` 与受保护 `/__auth_return` 路由。不要只新增 info 主路由而漏掉回调与交换路径，也不要把认证例外改成全局规则。匿名访问 `/`、`/api/status`、`/app.js` 均应进入 Stargate；登录后页面与 API 应可读，会话失效后重新受到保护。

内部证书长期有效不等于可以忽略私钥保护；到期提示也不替代异机告警。原始性能采样、私有快照与带实际地址的截图不要提交 GitHub。

Docker 重启计数来自 Docker `RestartCount`，不包含所有手工或宿主机修复程序发起的重启。自动修复另有受限的操作记录，不能只靠该计数判断是否修复过。

组件状态按“运行 / 健康 / 探测”展示：运行状态来自 Docker State，健康来自 Docker Healthcheck，探测是宿主机采集器额外执行的请求或心跳检查。`—` 表示未配置额外探测，不等于失败。目前 Traefik 使用容器内 `traefik healthcheck`，没有额外 HTTP 探针，因此正常显示为 `running / healthy / —`；其他组件显示 `ok` 是其额外探测通过。这些检查均不保证所有公开路由或发布结果正确。
