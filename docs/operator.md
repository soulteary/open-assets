# 日常操作入口

在部署服务器的 `/app/open-assets` 中使用 `sudo ./scripts/open-assets`。开发者本机继续通过 Git SSH Push 提交；本工具不替代个人 Git 密钥或 Gitea 权限。已有 `repo ensure`、`objects setup` 和容器内 `python -m open_assets` 命令继续可用。

## 先确认环境与 key

```sh
cd /app/open-assets
sudo ./scripts/open-assets doctor
sudo ./scripts/open-assets repo list
sudo ./scripts/open-assets status --key main-images
```

repo list 读取配置中的稳定 key、仓库名、分支和图片 URL 前缀，不调用 Gitea，也不需要管理员密码。key 不是 owner/repository 名称。doctor 校验配置结构、八个应用容器、八个 timer 与 Info 主机快照；不启动/重启服务、不初始化、不读取存储 root 密码。既有外部 Traefik、外部 DNS/防火墙、业务内容、备份恢复和低频缓存是否足够新仍需另查。

doctor 中“通过”是该检查当时满足条件，不是整套部署完全健康的证明。定时器 active 只说明调度已启用，不能代替服务最后执行结果。Info 快照新鲜也不表示对象/审计缓存刚刚更新。失败时按提示检查，避免重跑初始化来处理日常故障。

repo list、doctor、links 支持 --json。doctor 发现待处理项返回 1；正常返回 0。帮助与参数解析不需要 Docker、配置或 root；实际命令仍需相应文件/Docker 读取权限。

## 找到正确操作

日常任务查询：

```sh
sudo ./scripts/open-assets status --key main-images
sudo ./scripts/open-assets status all --key main-images --sha "$ASSET_COMMIT"
```

ASSET_COMMIT 是内容仓库中 `git rev-parse HEAD` 的完整 40 位小写输出，需要传到当前服务器终端。status 输出 ID、key、SHA、状态；只匹配完整 key/SHA。没有输出表示所选任务目录没有匹配项，不代表成功，也不代表内容不存在。查询旧任务时使用 all 或 archived。

- Push 已入队但未完成：先查 status，failed 才用 retry JOB_ID。publish 的返回值是任务 ID，还要继续确认 published。
- 需要主动发布某个历史提交：publish KEY SHA 会在任务成功后改变当前图片版本。已有终态任务仍按既有规则去重。
- 对象或本地历史副本被清理：restore-objects KEY SHA；需要同时重建本地时加 --with-local。默认保持当前版本，不靠重放终态 publish 任务恢复。
- 已有完整副本且要切换图片：rollback-images KEY SHA。需要从 Git 先重建、完整验证再切换时，使用 restore-objects KEY SHA --with-local --rollback。
- 恢复报告未完成或不确定：check-restore KEY SHA。核对只写独立报告，不能当作修复或结案。
- 人工确认需要结案：close-restore KEY SHA --reason '复核说明'，同锁重新核对通过后记录结案，原报告保留。
- 审计容量增长：restore-audit-list KEY [SHA]；restore-audit-archive KEY [SHA] 默认预览，--apply 才实际归档；restore-audit-verify 校验归档。

可在操作前运行对应 --help：

```sh
sudo ./scripts/open-assets restore-objects --help
sudo ./scripts/open-assets restore-audit-archive --help
sudo ./scripts/open-assets repo ensure --help
```

帮助只列该命令适用的参数。错误的 key/SHA、任务 ID、缺少参数、错误开关或超出范围，在读取配置和执行前拒绝；不依赖 Python traceback 解释用法。发布/恢复的保护条件、去重和权限不因入口统一而改变。

## 生成可复制链接

```sh
sudo ./scripts/open-assets links main-images "$ASSET_COMMIT" \
  'public/images/2026/11/07/中文 +%.png'
```

路径是相对源目录的原始文件名，也接受已配置的 public/ 等源前缀。工具使用私有 domains.json 和仓库图片前缀生成 URL，中文、空格、+、% 按路径编码一次；不要预先 URL 编码文件名。隐藏路径、绝对路径、目录逃逸和重复分隔符会被拒绝。

图片 URL 固定指向当前版本，不绑定传入 SHA；普通附件 URL 指向该 SHA。工具只拼接地址，不拉取 Git、不检查对象或网页存在；JSON 中 verified=false 明确表示未验证。先检查发布任务/恢复结果，再复制链接。改变 bucket、边缘路由或私有域名元数据后，需要另外确认路由实际已生效。

## 权限、退出和兼容

脚本在宿主机调用固定的 Publisher 命令白名单，不运行任意 shell 或容器命令。服务启动用的 worker/serve/once/render 不通过此入口开放；由 Compose 管理。repo ensure 与 objects setup 仍委派现有管理程序，仍需其完整管理员配置和 root 权限，不因只读新命令而获得额外权限。

--root 是全局参数，放在子命令前：`./scripts/open-assets --root /app/open-assets repo list`。它只选择部署目录，不将配方变成任意多实例支持。脚本不自动执行 docker compose up，不修复权限，也不创建缺失部署。

转发命令保留 Publisher 的输出和退出码：多数结果为 JSON，status 输出四列基本信息，资源超限失败时追加限制代码；用法错误为 2，实际失败通常为 1，锁忙也可能为 2，应结合返回内容区分。原有安全相关异常继续只给固定错误类型，未增加原始 SDK 请求、环境或凭据的输出。

长命令中断或 SSH 断开时，远端操作可能仍在执行。不要立即用第二个写命令“补救”；先查询任务、check-restore 或审计位置，并核对维护锁及私有报告。Ctrl-C 不是跨系统回滚。运行输出含仓库身份、SHA 和 URL，只保存在私有终端，不提交公开仓库。

本入口目前覆盖仓库接入、日常发布/恢复/审计及只读发现。本地/对象保留、强制统计采样、备份和证书更新仍使用各自文档中的专用工具；尤其对象清理使用独立身份，不能因入口简化而改用 Publisher 授予删除权限。
